//! `bateri package install`, `update` and `watch`: what a machine asks the bateri app for.
//!
//! **The machine decides.** It looks at its own folder, asks the app what is out there
//! (`package offer`), judges the answer itself (the signature, the key, the version) and only then
//! asks for the file (`package fetch`); the app answers and goes to the internet when it is asked.
//! The folder is written by [`kablo_package::Store::put`], which checks it all once more, so what
//! the app says is never taken for true.
//!
//! **A person is asked about a key at a terminal, or not at all.** A key kablo ships with, and the
//! key already approved for the program, need no asking. Any other key is the person's to approve
//! (`Trust it? [y/N]`, or `--trust KEY` from a program that asked them already): with no terminal to
//! ask at, it is refused. A program whose newer release came with another key stops where it is, once
//! told to the person, until `update NAME` is answered at a terminal.
//!
//! **A Production server starts with updating off.** When the app says the machine it answers for
//! is a Production server (`production` in the offer) and the first program is installed on a
//! machine that has none, `auto-off` is written before the program goes in (and taken back if it
//! does not, unless it was there), so that nothing there changes on its own until a person says so
//! (`bateri package auto on`, which stays). A later install does not touch it.
//!
//! **A watch that looks at nothing still asks something.** With updating off, or nothing to look
//! at, a round tells the app what the machine has instead, which is how it learns the app has gone
//! and ends (69) rather than resting for ever.
//!
//! Nothing here sleeps but [`Machine::watch`]: once a day, while the bridge that started it is
//! there (a process the bridge starts ends with it).

use std::time::Duration;

use kablo_bateri::message::{Reply, Request};
use kablo_bateri::package::{
    self, Answer, Fetch, KeyChanged, NAMES_MAX, Offer, Offered, Package, State,
};
use kablo_package::{Outcome, Put, Release, Standing, Store, Trust, Version, check_name, inspect};

use super::words::clean;
use super::{Asked, Done, Exit, Io, failure, unread};

/// How long `watch` waits between two rounds of asking, the whole of its idleness: it is woken by
/// nothing else, and it ends with the bridge that started it.
pub const CHECK_EVERY: Duration = Duration::from_secs(24 * 60 * 60);

/// The bateri app, as far as this module asks it.
pub trait Bateri {
    fn ask(&self, request: &Request) -> Result<Asked, Exit>;
}

/// The person at the machine.
pub trait Person {
    /// Whether there is a terminal to ask at.
    fn at_terminal(&self) -> bool;
    /// Asks a yes-or-no question and says whether the answer was yes.
    fn agrees(&mut self, question: &str) -> bool;
}

/// What one machine needs to bring programs to itself.
pub struct Machine<'a> {
    pub store: &'a Store,
    pub bateri: &'a dyn Bateri,
    pub person: &'a mut dyn Person,
    /// The kind of machine, as `app.json` names it in `files`.
    pub target: &'a str,
    /// Why bringing is switched off here, if it is.
    pub switched_off: Option<&'static str>,
    /// A name for the directory a file is brought to, new at each call.
    pub parts: &'a dyn Fn() -> String,
}

/// What came of looking at one program.
#[derive(Debug, PartialEq, Eq)]
enum Looked {
    /// It is the newest.
    Current(Version),
    /// A newer release is in.
    Updated { from: Version, to: Version },
    /// A newer release is signed with another key; the program stays.
    Held { key: String },
    /// Nothing could be asked of it (no source is written down).
    Skipped(&'static str),
}

/// How a key is to be judged.
enum Way {
    /// Go on: with this key approved just now, or with none to approve.
    Go(Option<String>),
    /// Do not: the person was not asked.
    Hold,
}

/// What `install` and `update` are given: the one program they name, and the key `--trust` says
/// to approve.
struct Words<'a> {
    name: Option<&'a str>,
    trust: Option<&'a str>,
}

/// Reads the words of `package VERB`: at most one name and one `--trust KEY`, wherever they stand.
/// `one_program` finishes the sentence that says what a second name is.
fn read_words<'a>(verb: &str, one_program: &str, args: &'a [String]) -> Result<Words<'a>, Exit> {
    let mut words = Words {
        name: None,
        trust: None,
    };
    let mut rest = args.iter();
    while let Some(word) = rest.next() {
        match word.as_str() {
            "--trust" => {
                if words.trust.is_some() {
                    return Err(Exit::mistake("--trust is given twice"));
                }
                // A key is not an option: `--trust --force` is a key left out.
                let key = rest
                    .next()
                    .filter(|key| !key.starts_with('-'))
                    .ok_or_else(|| Exit::mistake("--trust takes the key to approve"))?;
                words.trust = Some(key.as_str());
            }
            other if other.starts_with('-') => {
                return Err(Exit::mistake(format!(
                    "package {verb} takes no '{}'",
                    clean(other)
                )));
            }
            other if words.name.is_none() => words.name = Some(other),
            _ => {
                return Err(Exit::mistake(format!("package {verb} takes {one_program}")));
            }
        }
    }
    Ok(words)
}

impl Machine<'_> {
    fn allowed(&self) -> Done {
        match self.switched_off {
            Some(why) => Err(Exit::new(
                77,
                format!("this server does not allow it ({why})"),
            )),
            None => Ok(()),
        }
    }

    /// One `package` request and the reply, or the end of the command that the failure comes to.
    fn ask(&self, package: Package) -> Result<Reply, Exit> {
        match self.bateri.ask(&Request::Package(package))? {
            Asked::Reply(Reply::Failed(failed)) if package::is_old(&failed) => Err(Exit::failed(
                "this bateri does not bring programs yet; update bateri",
            )),
            Asked::Reply(Reply::Failed(failed)) => Err(failure(&failed)),
            Asked::Reply(reply) => Ok(reply),
            Asked::Unreadable => Err(unread()),
            Asked::NoApp | Asked::Silent => {
                Err(Exit::new(69, "no bateri is connected to bring it"))
            }
        }
    }

    /// The newest release of `source` newer than `have`, if there is one.
    fn offer(
        &self,
        source: &str,
        key: Option<String>,
        have: Option<Version>,
    ) -> Result<Option<Offered>, Exit> {
        let reply = self.ask(Package::Offer(Offer {
            source: source.to_owned(),
            target: self.target.to_owned(),
            key,
            have: have.map(|version| version.to_string()),
        }))?;
        match reply {
            Reply::Package(Answer::Current) => Ok(None),
            Reply::Package(Answer::Offered(offered)) => Ok(Some(offered)),
            _ => Err(unread()),
        }
    }

    /// Whether a release, signed with `release.key`, may be put in, and with whose approval.
    /// `may_ask` is whether a person may be asked (a command a person typed) and not a round
    /// nobody sees.
    fn way(&mut self, release: &Release, trust: Option<&str>, may_ask: bool) -> Result<Way, Exit> {
        let name = &release.name;
        let key = &release.key;
        // `--trust` approves the key it names and no other, whatever the standing of this one: a
        // person who named a key and meets another is not to be let through.
        if let Some(trusted) = trust
            && trusted != key
        {
            return Err(Exit::new(
                77,
                format!("{name} is signed with {key}, not with {}", clean(trusted)),
            ));
        }
        let said = match self
            .store
            .standing(name, key)
            .map_err(|error| Exit::failed(clean(&error.to_string())))?
        {
            Standing::Approved | Standing::Shipped => return Ok(Way::Go(None)),
            Standing::Unknown => format!(
                "{name} {} is signed with {key}, which this machine does not know.",
                release.version
            ),
            Standing::Changed { recorded } => format!(
                "{name} {} is signed with {key}, not with {recorded}, which was approved for it.",
                release.version
            ),
        };
        if let Some(trusted) = trust {
            return if trusted == key {
                Ok(Way::Go(Some(key.clone())))
            } else {
                Err(Exit::new(
                    77,
                    format!("{name} is signed with {key}, not with {}", clean(trusted)),
                ))
            };
        }
        if !may_ask {
            return Ok(Way::Hold);
        }
        if !self.person.at_terminal() {
            return Err(Exit::new(
                77,
                format!(
                    "{said} Nobody can be asked here: run it in a terminal, or give --trust {key} if the key is known to be right"
                ),
            ));
        }
        if self.person.agrees(&format!("{said} Trust it? [y/N] ")) {
            Ok(Way::Go(Some(key.clone())))
        } else {
            Err(Exit::new(77, format!("{name} is left as it is")))
        }
    }

    /// Brings the file of the release and puts it in.
    fn bring(
        &self,
        offered: &Offered,
        release: &Release,
        approved: Option<&str>,
    ) -> Result<Outcome, Exit> {
        let part = (self.parts)();
        let reply = self.ask(Package::Fetch(Fetch {
            name: release.name.clone(),
            version: release.version.to_string(),
            target: self.target.to_owned(),
            source: offered.source.clone(),
            part: part.clone(),
        }))?;
        if !matches!(reply, Reply::Package(Answer::Brought)) {
            return Err(unread());
        }
        self.store
            .put(&Put {
                name: &release.name,
                part: &part,
                manifest: offered.manifest.as_bytes(),
                signature: offered.signature.as_bytes(),
                target: self.target,
                source: Some(&offered.source),
                trust: approved.map_or(Trust::Recorded, Trust::Approved),
            })
            .map_err(|error| Exit::failed(clean(&error.to_string())))
    }

    /// `install SOURCE [--trust KEY]`.
    pub fn install(&mut self, io: &mut Io<'_>, args: &[String]) -> Done {
        let Words {
            name: source,
            trust,
        } = read_words(
            "install",
            "one program: a name, user/repo or an address",
            args,
        )?;
        let Some(source) = source else {
            return Err(Exit::mistake(
                "package install takes a program: a name, user/repo or an address",
            ));
        };
        self.allowed()?;
        let Some(offered) = self.offer(source, None, None)? else {
            return Err(unread());
        };
        let release = inspect(offered.manifest.as_bytes(), offered.signature.as_bytes())
            .map_err(|error| Exit::failed(clean(&error.to_string())))?;
        check_name(&release.name).map_err(|error| Exit::failed(clean(&error.to_string())))?;
        let current = self
            .store
            .current(&release.name)
            .map_err(|error| Exit::failed(clean(&error.to_string())))?;
        if let Some(current) = current.filter(|current| *current >= release.version) {
            let _ = writeln!(io.out, "{} {current} is here already", release.name);
            return Ok(());
        }
        let Way::Go(approved) = self.way(&release, trust, true)? else {
            return Err(unread());
        };
        // The first program of a Production server: read from the folder before it is put in.
        // Updating is left off before the program goes in and taken back if it does not, so that
        // an install that goes in never finds the note unwritten.
        let first_on_production = offered.production
            && self
                .store
                .names()
                .map_err(|error| Exit::failed(clean(&error.to_string())))?
                .is_empty();
        let was_on = self.store.auto();
        if first_on_production {
            self.store.set_auto(false).map_err(|error| {
                Exit::failed(format!(
                    "could not leave updating off on this Production server: {}",
                    clean(&error.to_string())
                ))
            })?;
        }
        if let Err(exit) = self.bring(&offered, &release, approved.as_deref()) {
            if first_on_production && was_on {
                // Nothing came in, so what was off for it is on again; if that cannot be written it
                // stays off, the safe side.
                let _ = self.store.set_auto(true);
            }
            return Err(exit);
        }
        let _ = writeln!(io.out, "{} {} installed", release.name, release.version);
        if first_on_production {
            let _ = writeln!(
                io.out,
                "programs are not updated on their own on this server, which bateri keeps as Production; `bateri package auto on` turns it on"
            );
        }
        Ok(())
    }

    /// `update [NAME [--trust KEY]]`.
    pub fn update(&mut self, io: &mut Io<'_>, args: &[String]) -> Done {
        let Words { name: named, trust } = read_words("update", "the name of one program", args)?;
        if trust.is_some() && named.is_none() {
            return Err(Exit::mistake(
                "package update --trust names the program whose key it approves: update NAME --trust KEY",
            ));
        }
        self.allowed()?;
        let names = match named {
            Some(name) => vec![name.to_owned()],
            None => self
                .store
                .names()
                .map_err(|error| Exit::failed(clean(&error.to_string())))?,
        };
        if names.is_empty() {
            let _ = writeln!(io.out, "no programs here");
        }
        let mut trouble = false;
        for name in names {
            match self.look(&name, named.is_some(), trust) {
                Ok(Looked::Current(version)) => {
                    let _ = writeln!(io.out, "{name} {version} is the newest");
                }
                Ok(Looked::Updated { from, to }) => {
                    let _ = writeln!(io.out, "{name} {from} -> {to}");
                }
                Ok(Looked::Held { key }) => {
                    let _ = writeln!(
                        io.out,
                        "{name} stays as it is: its newer release is signed with {key}, not with the approved key; `bateri package update {name}` asks about it at a terminal, or, if the key is known to be right, `bateri package update {name} --trust {key}` approves it"
                    );
                }
                Ok(Looked::Skipped(why)) => {
                    let _ = writeln!(io.out, "{name}: {why}");
                }
                Err(exit) if exit.code == 69 || exit.code > 128 => return Err(exit),
                Err(exit) if named.is_some() => return Err(exit),
                Err(exit) => {
                    trouble = true;
                    if let Some(text) = exit.text {
                        io.say(&format!("{name}: {text}"));
                    }
                }
            }
        }
        if trouble {
            Err(Exit::status(1))
        } else {
            Ok(())
        }
    }

    /// Looks for a newer release of one program and puts it in. `may_ask` is whether a person
    /// may be asked about a key; `trust` is the key a person named to be approved.
    fn look(&mut self, name: &str, may_ask: bool, trust: Option<&str>) -> Result<Looked, Exit> {
        let trouble = |error: kablo_package::Error| Exit::failed(clean(&error.to_string()));
        let Some(current) = self.store.current(name).map_err(trouble)? else {
            return Err(Exit::failed(format!("{} is not here", clean(name))));
        };
        let Some(source) = self.store.source(name).map_err(trouble)? else {
            return Ok(Looked::Skipped("it has no source written down to look at"));
        };
        // The key the app is told is the one a person named to approve, when one did: the one it
        // would otherwise leave a release out for is the one about to be given up.
        let key = match trust {
            Some(trusted) => Some(trusted.to_owned()),
            None => self.store.key(name).map_err(trouble)?,
        };
        let Some(offered) = self.offer(&source, key, Some(current))? else {
            return Ok(Looked::Current(current));
        };
        let release =
            inspect(offered.manifest.as_bytes(), offered.signature.as_bytes()).map_err(trouble)?;
        if release.name != name {
            return Err(Exit::failed(format!(
                "the release is of {}, not of {name}",
                clean(&release.name)
            )));
        }
        // What the app says is judged here: a release that is not newer is not one.
        if release.version <= current {
            return Ok(Looked::Current(current));
        }
        match self.way(&release, trust, may_ask)? {
            Way::Hold => {
                if self.store.stop(name, &release.key).map_err(trouble)? {
                    // Once: nothing is said again for the same key.
                    let _ = self.ask(Package::KeyChanged(KeyChanged {
                        name: name.to_owned(),
                        key: release.key.clone(),
                    }));
                }
                Ok(Looked::Held { key: release.key })
            }
            Way::Go(approved) => {
                self.bring(&offered, &release, approved.as_deref())?;
                Ok(Looked::Updated {
                    from: current,
                    to: release.version,
                })
            }
        }
    }

    /// `watch`: tells the app what the machine has, and then, as long as `rest` says to go on,
    /// looks for newer releases of what is here (unless the person turned that off) and rests.
    /// `rest` is where the day passes; a false from it ends the watch.
    pub fn watch(&mut self, rest: &mut dyn FnMut() -> bool) -> Done {
        self.allowed()?;
        self.tell()?;
        let mut just_told = true;
        loop {
            let asked = if self.store.auto() {
                self.round()?
            } else {
                false
            };
            // A round that asked the app nothing (updating is off, or there is nothing to look at)
            // tells it what the machine has instead, as the day's word: it is how a watch finds the
            // app gone, and ends, and not rests for ever in a process nobody started.
            if !asked && !just_told {
                self.tell_again()?;
            }
            just_told = false;
            if !rest() {
                return Ok(());
            }
        }
    }

    /// `package state` again: only the end of the app ends the watch, any other trouble waits for
    /// the next round.
    fn tell_again(&self) -> Done {
        match self.tell() {
            Err(exit) if exit.code == 69 || exit.code > 128 => Err(exit),
            _ => Ok(()),
        }
    }

    /// `package state`.
    fn tell(&self) -> Done {
        let trouble = |error: kablo_package::Error| Exit::failed(clean(&error.to_string()));
        let mut installed = self.store.names().map_err(trouble)?;
        installed.truncate(NAMES_MAX);
        let mut not_here = self.store.not_here().map_err(trouble)?;
        not_here.truncate(NAMES_MAX);
        let reply = self.ask(Package::State(State {
            target: self.target.to_owned(),
            installed,
            not_here,
        }))?;
        match reply {
            Reply::Done => Ok(()),
            _ => Err(unread()),
        }
    }

    /// One round of looking at every program, for nobody to read: a program that cannot be looked
    /// at is looked at again in the next round, and only the end of the app ends the watch.
    /// Says whether the app was asked anything.
    fn round(&mut self) -> Result<bool, Exit> {
        let names = self
            .store
            .names()
            .map_err(|error| Exit::failed(clean(&error.to_string())))?;
        let mut asked = false;
        for name in names {
            match self.look(&name, false, None) {
                Err(exit) if exit.code == 69 || exit.code > 128 => return Err(exit),
                Ok(Looked::Skipped(_)) => {}
                _ => asked = true,
            }
        }
        Ok(asked)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::rc::Rc;

    use kablo_bateri::message::{Failure, Kind};

    use super::*;

    const TARGET: &str = "aarch64-apple-darwin";

    fn fixture(file: &str) -> String {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../kablo-package/tests/fixture");
        fs::read_to_string(dir.join(file))
            .unwrap()
            .trim_end_matches('\n')
            .to_owned()
    }

    /// The key of author A in the fixtures of `kablo-package`, the one the machine trusts.
    fn key_a() -> String {
        fixture("key-a.pub")
    }

    fn key_b() -> String {
        fixture("key-b.pub")
    }

    /// The release a fixture is: its `app.json` and signature exactly (no line break is cut from them).
    fn release(id: &str) -> (String, String) {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../kablo-package/tests/fixture");
        (
            fs::read_to_string(dir.join(format!("{id}.app.json"))).unwrap(),
            fs::read_to_string(dir.join(format!("{id}.app.json.sig"))).unwrap(),
        )
    }

    fn version_of(id: &str) -> Version {
        let (manifest, signature) = release(id);
        inspect(manifest.as_bytes(), signature.as_bytes())
            .unwrap()
            .version
    }

    fn v(text: &str) -> Version {
        Version::parse(text).unwrap()
    }

    /// What the app says: a catalog of fixtures under one source, and a record of what it was asked.
    struct Fake {
        root: PathBuf,
        /// The fixture ids on offer.
        catalog: RefCell<Vec<&'static str>>,
        told: RefCell<Vec<Package>>,
        /// How it answers: `Some` is what every request gets instead.
        refuses: Rc<RefCell<Option<Asked>>>,
        /// A file that is not the one signed.
        wrong_file: Cell<bool>,
        /// The release it offers is another program's.
        lies_about_the_name: Cell<bool>,
        /// It offers what it has, whatever version the machine said it had.
        ignores_have: Cell<bool>,
        /// It keeps the machine as a Production server, and says so in every offer.
        production: Cell<bool>,
    }

    impl Fake {
        fn told(&self) -> Vec<Package> {
            self.told.borrow().clone()
        }

        fn told_verbs(&self) -> Vec<&'static str> {
            self.told.borrow().iter().map(Package::verb).collect()
        }
    }

    impl Bateri for Fake {
        fn ask(&self, request: &Request) -> Result<Asked, Exit> {
            let Request::Package(package) = request else {
                return Ok(Asked::Silent);
            };
            self.told.borrow_mut().push(package.clone());
            if let Some(asked) = self.refuses.borrow_mut().take() {
                return Ok(asked);
            }
            Ok(Asked::Reply(match package {
                Package::State(_) | Package::KeyChanged(_) => Reply::Done,
                Package::Offer(offer) => {
                    let have = offer
                        .have
                        .as_deref()
                        .and_then(Version::parse)
                        .filter(|_| !self.ignores_have.get());
                    let newest = self
                        .catalog
                        .borrow()
                        .iter()
                        .copied()
                        .filter(|id| have.is_none_or(|have| version_of(id) > have))
                        .max_by_key(|id| version_of(id));
                    match newest {
                        None => Reply::Package(Answer::Current),
                        Some(id) => {
                            let (mut manifest, signature) = release(id);
                            if self.lies_about_the_name.get() {
                                manifest = manifest.replace("textpen", "otherpen");
                            }
                            Reply::Package(Answer::Offered(Offered {
                                source: "bateri/textpen".into(),
                                manifest,
                                signature,
                                production: self.production.get(),
                            }))
                        }
                    }
                }
                Package::Fetch(fetch) => {
                    let dir = self
                        .root
                        .join(&fetch.name)
                        .join(format!(".{}.part-{}", fetch.version, fetch.part));
                    fs::create_dir_all(&dir).unwrap();
                    let body = if self.wrong_file.get() {
                        "#!/bin/sh\necho not what was signed\n".to_owned()
                    } else {
                        format!(
                            "#!/bin/sh\necho {} {} {}\n",
                            fetch.name, fetch.version, fetch.target
                        )
                    };
                    fs::write(dir.join(&fetch.name), body).unwrap();
                    Reply::Package(Answer::Brought)
                }
            }))
        }
    }

    struct Scripted {
        terminal: bool,
        answers: Vec<bool>,
        asked: Vec<String>,
    }

    impl Person for Scripted {
        fn at_terminal(&self) -> bool {
            self.terminal
        }

        fn agrees(&mut self, question: &str) -> bool {
            self.asked.push(question.to_owned());
            if self.answers.is_empty() {
                false
            } else {
                self.answers.remove(0)
            }
        }
    }

    /// A machine in a directory of its own, and an app to ask.
    struct Rig {
        dir: PathBuf,
        store: Store,
        fake: Fake,
        person: Scripted,
        switched_off: Option<&'static str>,
        parts: Cell<u32>,
    }

    impl Rig {
        /// `embedded` are the keys the machine trusts without asking, `catalog` the fixtures on offer.
        fn new(label: &str, embedded: &[&str], catalog: &[&'static str]) -> Rig {
            let dir = std::env::temp_dir().join(format!("kbring-{}-{label}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            let root = dir.join("apps");
            Rig {
                store: Store::new(root.clone(), embedded),
                fake: Fake {
                    root,
                    catalog: RefCell::new(catalog.to_vec()),
                    told: RefCell::new(Vec::new()),
                    refuses: Rc::new(RefCell::new(None)),
                    wrong_file: Cell::new(false),
                    lies_about_the_name: Cell::new(false),
                    ignores_have: Cell::new(false),
                    production: Cell::new(false),
                },
                person: Scripted {
                    terminal: false,
                    answers: Vec::new(),
                    asked: Vec::new(),
                },
                switched_off: None,
                parts: Cell::new(0),
                dir,
            }
        }

        fn trusting_a(label: &str, catalog: &[&'static str]) -> Rig {
            Rig::new(label, &[key_a().as_str()], catalog)
        }

        /// Runs `f` on a machine and returns what it came to with what it printed and said.
        fn run<R>(
            &mut self,
            f: impl FnOnce(&mut Machine<'_>, &mut Io<'_>) -> R,
        ) -> (R, String, String) {
            let parts = || {
                self.parts.set(self.parts.get() + 1);
                format!("p{}", self.parts.get())
            };
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let result = {
                let mut machine = Machine {
                    store: &self.store,
                    bateri: &self.fake,
                    person: &mut self.person,
                    target: TARGET,
                    switched_off: self.switched_off,
                    parts: &parts,
                };
                let mut io = Io {
                    out: &mut out,
                    err: &mut err,
                };
                f(&mut machine, &mut io)
            };
            (
                result,
                String::from_utf8(out).unwrap(),
                String::from_utf8(err).unwrap(),
            )
        }

        fn install(&mut self, args: &[&str]) -> (Done, String, String) {
            let args: Vec<String> = args.iter().map(|word| (*word).to_owned()).collect();
            self.run(|machine, io| machine.install(io, &args))
        }

        fn update(&mut self, args: &[&str]) -> (Done, String, String) {
            let args: Vec<String> = args.iter().map(|word| (*word).to_owned()).collect();
            self.run(|machine, io| machine.update(io, &args))
        }

        /// `watch`, for `rounds` rounds.
        fn watch(&mut self, rounds: u32) -> Done {
            let mut left = rounds;
            self.run(|machine, _| {
                machine.watch(&mut || {
                    left -= 1;
                    left > 0
                })
            })
            .0
        }

        fn current(&self, name: &str) -> Option<Version> {
            self.store.current(name).unwrap()
        }
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn code(done: &Done) -> u8 {
        match done {
            Ok(()) => 0,
            Err(exit) => exit.code,
        }
    }

    fn text(done: &Done) -> String {
        match done {
            Ok(()) => String::new(),
            Err(exit) => exit.text.clone().unwrap_or_default(),
        }
    }

    #[test]
    fn install_asks_judges_fetches_and_puts_in_a_release_signed_with_a_shipped_key() {
        let mut rig = Rig::trusting_a("install", &["textpen-1.0.0", "textpen-1.1.0"]);
        let (done, out, _) = rig.install(&["textpen"]);
        assert_eq!(code(&done), 0, "{}", text(&done));
        assert_eq!(out, "textpen 1.1.0 installed\n");
        assert_eq!(rig.current("textpen"), Some(v("1.1.0")));
        // What it asked: the source as the person wrote it, nothing known about the program yet,
        // and then the file of the release it chose.
        let told = rig.fake.told();
        assert_eq!(
            told[0],
            Package::Offer(Offer {
                source: "textpen".into(),
                target: TARGET.into(),
                key: None,
                have: None
            })
        );
        assert_eq!(
            told[1],
            Package::Fetch(Fetch {
                name: "textpen".into(),
                version: "1.1.0".into(),
                target: TARGET.into(),
                source: "bateri/textpen".into(),
                part: "p1".into()
            })
        );
        assert_eq!(told.len(), 2);
        // Where it came from is what the app resolved, and nobody was asked about the key.
        assert_eq!(
            rig.store.source("textpen").unwrap().as_deref(),
            Some("bateri/textpen")
        );
        assert_eq!(rig.store.key("textpen").unwrap(), Some(key_a()));
        assert!(rig.person.asked.is_empty());
    }

    #[test]
    fn a_program_that_is_here_at_that_version_is_not_brought_again() {
        let mut rig = Rig::trusting_a("again", &["textpen-1.0.0"]);
        assert_eq!(code(&rig.install(&["textpen"]).0), 0);
        rig.fake.told.borrow_mut().clear();
        let (done, out, _) = rig.install(&["textpen"]);
        assert_eq!(code(&done), 0);
        assert_eq!(out, "textpen 1.0.0 is here already\n");
        assert_eq!(rig.fake.told_verbs(), ["package offer"]);
    }

    #[test]
    fn a_key_nobody_knows_is_asked_about_at_a_terminal_and_refused_without_one() {
        let mut rig = Rig::new("unknown", &[], &["textpen-1.0.0"]);
        // No terminal: refused before anything is brought, with the way out in the words.
        let (done, _, _) = rig.install(&["textpen"]);
        assert_eq!(code(&done), 77);
        assert!(
            text(&done).contains(&format!("--trust {}", key_a())),
            "{}",
            text(&done)
        );
        assert_eq!(rig.fake.told_verbs(), ["package offer"]);
        assert_eq!(rig.current("textpen"), None);

        // A terminal and a no.
        rig.person.terminal = true;
        rig.person.answers = vec![false];
        let (done, _, _) = rig.install(&["textpen"]);
        assert_eq!(code(&done), 77);
        assert_eq!(rig.current("textpen"), None);
        assert!(
            rig.person.asked[0].contains(&key_a()),
            "{:?}",
            rig.person.asked
        );
        assert!(rig.person.asked[0].ends_with("Trust it? [y/N] "));

        // A terminal and a yes: the key is kept for the program.
        rig.person.answers = vec![true];
        let (done, out, _) = rig.install(&["textpen"]);
        assert_eq!(code(&done), 0, "{}", text(&done));
        assert_eq!(out, "textpen 1.0.0 installed\n");
        assert_eq!(rig.store.key("textpen").unwrap(), Some(key_a()));
    }

    #[test]
    fn trust_approves_exactly_the_key_it_names() {
        let mut rig = Rig::new("trust", &[], &["textpen-1.0.0"]);
        let (done, _, _) = rig.install(&["textpen", "--trust", &key_b()]);
        assert_eq!(code(&done), 77);
        assert!(text(&done).contains(&key_b()), "{}", text(&done));
        assert_eq!(rig.current("textpen"), None);
        assert!(rig.person.asked.is_empty());
        let (done, _, _) = rig.install(&["--trust", &key_a(), "textpen"]);
        assert_eq!(code(&done), 0, "{}", text(&done));
        assert_eq!(rig.store.key("textpen").unwrap(), Some(key_a()));
        assert!(rig.person.asked.is_empty());
    }

    #[test]
    fn install_takes_one_program_and_nothing_it_does_not_know() {
        let mut rig = Rig::trusting_a("words", &["textpen-1.0.0"]);
        for args in [
            &[][..],
            &["a", "b"],
            &["--force", "textpen"],
            &["textpen", "--trust"],
        ] {
            assert_eq!(code(&rig.install(args).0), 64, "{args:?}");
        }
        assert!(rig.fake.told().is_empty());
    }

    #[test]
    fn no_bateri_and_an_old_bateri_and_a_server_that_does_not_allow_it_each_say_so() {
        let mut rig = Rig::trusting_a("ends", &["textpen-1.0.0"]);
        *rig.fake.refuses.borrow_mut() = Some(Asked::NoApp);
        let (done, _, _) = rig.install(&["textpen"]);
        assert_eq!(code(&done), 69);
        assert_eq!(text(&done), "no bateri is connected to bring it");

        let old = Failure::new(
            Kind::Usage,
            "`package offer` is no verb: place, ls, close, notify",
        );
        *rig.fake.refuses.borrow_mut() = Some(Asked::Reply(Reply::Failed(old)));
        let (done, _, _) = rig.install(&["textpen"]);
        assert_eq!(code(&done), 1);
        assert!(text(&done).contains("update bateri"), "{}", text(&done));

        let failed = Failure::new(Kind::Failed, "the source did not answer");
        *rig.fake.refuses.borrow_mut() = Some(Asked::Reply(Reply::Failed(failed)));
        let (done, _, _) = rig.install(&["textpen"]);
        assert_eq!(code(&done), 1);
        assert_eq!(text(&done), "the source did not answer");

        rig.fake.told.borrow_mut().clear();
        rig.switched_off = Some("turned off by the server's administrator");
        for done in [rig.install(&["textpen"]).0, rig.update(&[]).0, rig.watch(1)] {
            assert_eq!(code(&done), 77);
            assert!(
                text(&done).starts_with("this server does not allow it"),
                "{}",
                text(&done)
            );
        }
        assert!(rig.fake.told().is_empty());
        assert_eq!(rig.current("textpen"), None);
    }

    #[test]
    fn what_the_app_offers_is_judged_here() {
        let mut rig = Rig::trusting_a("judged", &["textpen-1.0.0"]);
        // A release of another program than the one the folder is for.
        rig.fake.lies_about_the_name.set(true);
        let (done, _, _) = rig.install(&["textpen"]);
        assert_eq!(code(&done), 1);
        assert!(text(&done).contains("signature"), "{}", text(&done));
        rig.fake.lies_about_the_name.set(false);

        // A file that is not the one signed: refused by the folder, which is left as it was.
        rig.fake.wrong_file.set(true);
        let (done, _, _) = rig.install(&["textpen"]);
        assert_eq!(code(&done), 1);
        assert!(text(&done).contains("sum"), "{}", text(&done));
        assert_eq!(rig.current("textpen"), None);
        rig.fake.wrong_file.set(false);

        let (done, _, _) = rig.install(&["textpen"]);
        assert_eq!(code(&done), 0);
        // An app that offers an older release than what is here has offered nothing.
        *rig.fake.catalog.borrow_mut() = vec!["textpen-0.9.0"];
        rig.fake.ignores_have.set(true);
        rig.fake.told.borrow_mut().clear();
        let (done, out, _) = rig.update(&["textpen"]);
        assert_eq!(code(&done), 0);
        assert_eq!(out, "textpen 1.0.0 is the newest\n");
        assert_eq!(rig.fake.told_verbs(), ["package offer"]);
        assert_eq!(rig.current("textpen"), Some(v("1.0.0")));
    }

    #[test]
    fn update_brings_a_newer_release() {
        let mut rig = Rig::trusting_a("update", &["textpen-1.0.0"]);
        assert_eq!(code(&rig.install(&["textpen"]).0), 0);
        let (done, out, _) = rig.update(&[]);
        assert_eq!(code(&done), 0);
        assert_eq!(out, "textpen 1.0.0 is the newest\n");
        // The question carries what the machine knows: the source it wrote down, the key
        // approved for the program, the version it has.
        let told = rig.fake.told();
        assert_eq!(
            told.last(),
            Some(&Package::Offer(Offer {
                source: "bateri/textpen".into(),
                target: TARGET.into(),
                key: Some(key_a()),
                have: Some("1.0.0".into())
            }))
        );
        rig.fake.catalog.borrow_mut().push("textpen-1.2.0");
        let (done, out, _) = rig.update(&["textpen"]);
        assert_eq!(code(&done), 0, "{}", text(&done));
        assert_eq!(out, "textpen 1.0.0 -> 1.2.0\n");
        assert_eq!(rig.current("textpen"), Some(v("1.2.0")));
        // A program that is not here is the command's mistake; no program is no trouble.
        assert_eq!(code(&rig.update(&["ghost"]).0), 1);
        assert_eq!(code(&rig.update(&["a", "b"]).0), 64);
        assert_eq!(code(&rig.update(&["--all"]).0), 64);
    }

    #[test]
    fn a_program_without_a_source_is_skipped_and_nothing_is_asked() {
        let mut rig = Rig::trusting_a("nosource", &["textpen-1.0.0"]);
        assert_eq!(code(&rig.install(&["textpen"]).0), 0);
        fs::remove_file(rig.store.root().join("textpen/source")).unwrap();
        rig.fake.told.borrow_mut().clear();
        let (done, out, _) = rig.update(&[]);
        assert_eq!(code(&done), 0);
        assert_eq!(out, "textpen: it has no source written down to look at\n");
        assert!(rig.fake.told().is_empty());
    }

    #[test]
    fn the_first_program_of_a_production_server_leaves_updating_off_and_says_so() {
        let mut rig = Rig::trusting_a("production", &["textpen-1.0.0"]);
        rig.fake.production.set(true);
        assert!(
            rig.store.auto(),
            "updating is on until something turns it off"
        );
        let (done, out, _) = rig.install(&["textpen"]);
        assert_eq!(code(&done), 0, "{}", text(&done));
        assert!(!rig.store.auto());
        assert!(out.starts_with("textpen 1.0.0 installed\n"), "{out}");
        assert!(out.contains("bateri package auto on"), "{out}");
    }

    #[test]
    fn a_machine_that_is_not_a_production_server_keeps_updating_on() {
        let mut rig = Rig::trusting_a("not-production", &["textpen-1.0.0"]);
        let (done, out, _) = rig.install(&["textpen"]);
        assert_eq!(code(&done), 0, "{}", text(&done));
        assert!(rig.store.auto());
        assert_eq!(out, "textpen 1.0.0 installed\n");
    }

    #[test]
    fn a_later_install_on_a_production_server_leaves_what_the_person_chose() {
        let mut rig = Rig::trusting_a("production-later", &["textpen-1.0.0"]);
        rig.fake.production.set(true);
        assert_eq!(code(&rig.install(&["textpen"]).0), 0);
        assert!(!rig.store.auto());
        // The person turns updating on; the next install, of the same program or another, is not
        // the first and does not take that back.
        rig.store.set_auto(true).unwrap();
        rig.fake.catalog.borrow_mut().push("textpen-1.1.0");
        let (done, out, _) = rig.install(&["textpen"]);
        assert_eq!(code(&done), 0, "{}", text(&done));
        assert_eq!(out, "textpen 1.1.0 installed\n");
        assert!(rig.store.auto());
        *rig.fake.catalog.borrow_mut() = vec!["other-1.0.0"];
        let (done, _, _) = rig.install(&["other"]);
        assert_eq!(code(&done), 0, "{}", text(&done));
        assert!(rig.store.auto());
        // And a person who turned it off on purpose keeps it off, of course.
        rig.store.set_auto(false).unwrap();
        *rig.fake.catalog.borrow_mut() = vec!["textpen-1.2.0"];
        assert_eq!(code(&rig.install(&["textpen"]).0), 0);
        assert!(!rig.store.auto());
    }

    #[test]
    fn an_install_that_fails_on_a_production_server_leaves_it_as_it_was() {
        let mut rig = Rig::trusting_a("production-fails", &["textpen-1.0.0"]);
        rig.fake.production.set(true);
        rig.fake.wrong_file.set(true);
        let (done, _, _) = rig.install(&["textpen"]);
        assert_ne!(code(&done), 0);
        assert_eq!(rig.current("textpen"), None);
        assert!(
            rig.store.auto(),
            "nothing was installed, nothing is switched"
        );
        // The machine is still empty, so the install that works is its first.
        rig.fake.wrong_file.set(false);
        assert_eq!(code(&rig.install(&["textpen"]).0), 0);
        assert!(!rig.store.auto());
    }

    #[test]
    fn update_trust_approves_exactly_the_key_it_names_where_nobody_can_be_asked() {
        let mut rig = Rig::trusting_a("update-trust", &["textpen-1.0.0"]);
        assert_eq!(code(&rig.install(&["textpen"]).0), 0);
        rig.fake
            .catalog
            .borrow_mut()
            .push("textpen-1.3.0-other-key");

        // The key of the release is B: A is not it.
        let (done, _, _) = rig.update(&["textpen", "--trust", &key_a()]);
        assert_eq!(code(&done), 77);
        assert!(
            text(&done).contains(&key_b()) && text(&done).contains(&key_a()),
            "{}",
            text(&done)
        );
        assert_eq!(rig.current("textpen"), Some(v("1.0.0")));

        // B is: the program goes forward, with the key kept and nobody asked.
        let (done, out, _) = rig.update(&["--trust", &key_b(), "textpen"]);
        assert_eq!(code(&done), 0, "{}", text(&done));
        assert_eq!(out, "textpen 1.0.0 -> 1.3.0\n");
        assert_eq!(rig.store.key("textpen").unwrap(), Some(key_b()));
        assert_eq!(rig.store.stopped("textpen").unwrap(), None);
        assert!(rig.person.asked.is_empty());
    }

    #[test]
    fn update_trust_names_one_program_and_one_key() {
        let mut rig = Rig::trusting_a("update-trust-words", &["textpen-1.0.0"]);
        let (done, _, _) = rig.update(&["--trust", &key_b()]);
        assert_ne!(code(&done), 0);
        assert!(
            text(&done).contains("update NAME --trust KEY"),
            "{}",
            text(&done)
        );
        let (done, _, _) = rig.update(&["textpen", "--trust"]);
        assert!(
            text(&done).contains("takes the key to approve"),
            "{}",
            text(&done)
        );
        let (done, _, _) = rig.update(&["textpen", "other"]);
        assert!(text(&done).contains("one program"), "{}", text(&done));
        let (done, _, _) = rig.update(&["--force"]);
        assert!(
            text(&done).contains("takes no '--force'"),
            "{}",
            text(&done)
        );
        assert!(
            rig.fake.told().is_empty(),
            "a mistake in the words asks nobody"
        );
    }

    #[test]
    fn a_watch_that_looks_at_nothing_still_ends_when_the_app_is_gone() {
        // Updating off, and a machine with nothing on it with updating on: no round asks the app
        // anything, and the watch must still find that it has gone and end, not rest for ever.
        for (label, programs, auto) in [
            ("gone-off", true, false),
            ("gone-empty", false, true),
            ("gone-off-empty", false, false),
        ] {
            let mut rig = Rig::trusting_a(label, &["textpen-1.0.0"]);
            if programs {
                assert_eq!(code(&rig.install(&["textpen"]).0), 0, "{label}");
            }
            rig.store.set_auto(auto).unwrap();
            let mut rounds = 0;
            let refuses = Rc::clone(&rig.fake.refuses);
            let (done, ..) = rig.run(|machine, _| {
                machine.watch(&mut || {
                    rounds += 1;
                    assert!(rounds < 100, "{label}: the watch never found the app gone");
                    *refuses.borrow_mut() = Some(Asked::NoApp);
                    true
                })
            });
            assert_eq!(code(&done), 69, "{label}");
            assert_eq!(rounds, 1, "{label}");
        }
    }

    #[test]
    fn a_trouble_in_the_day_s_state_does_not_end_a_watch_that_looks_at_nothing() {
        let mut rig = Rig::trusting_a("state-trouble", &["textpen-1.0.0"]);
        rig.store.set_auto(false).unwrap();
        let failed = Failure::new(Kind::Failed, "the app could not take it");
        let mut rounds = 0;
        let refuses = Rc::clone(&rig.fake.refuses);
        let (done, ..) = rig.run(|machine, _| {
            machine.watch(&mut || {
                rounds += 1;
                if rounds == 1 {
                    *refuses.borrow_mut() = Some(Asked::Reply(Reply::Failed(failed.clone())));
                }
                rounds < 3
            })
        });
        assert_eq!(code(&done), 0);
        assert_eq!(rounds, 3);
    }

    #[test]
    fn a_production_server_whose_note_cannot_be_written_gets_nothing_installed() {
        let mut rig = Rig::trusting_a("production-unwritable", &["textpen-1.0.0"]);
        rig.fake.production.set(true);
        // `auto-off` stands where a directory is, so it cannot be written as a note.
        fs::create_dir_all(rig.dir.join("apps").join("auto-off")).unwrap();
        let (done, _, _) = rig.install(&["textpen"]);
        assert_ne!(code(&done), 0);
        assert!(
            text(&done).contains("could not leave updating off"),
            "{}",
            text(&done)
        );
        assert_eq!(rig.current("textpen"), None);
        assert_eq!(
            rig.fake.told_verbs(),
            ["package offer"],
            "no file was fetched"
        );
    }

    #[test]
    fn a_person_who_was_off_before_stays_off_when_the_first_install_on_production_fails() {
        let mut rig = Rig::trusting_a("production-was-off", &["textpen-1.0.0"]);
        rig.fake.production.set(true);
        rig.store.set_auto(false).unwrap();
        rig.fake.wrong_file.set(true);
        assert_ne!(code(&rig.install(&["textpen"]).0), 0);
        assert!(
            !rig.store.auto(),
            "what the person left off is not turned on"
        );
    }

    #[test]
    fn trust_names_the_key_of_the_release_whatever_its_standing() {
        // Key A is shipped, so nobody needs to be asked; B is not what the release is signed with.
        let mut rig = Rig::trusting_a("trust-standing", &["textpen-1.0.0"]);
        let (done, _, _) = rig.install(&["textpen", "--trust", &key_b()]);
        assert_eq!(code(&done), 77);
        assert!(text(&done).contains(&key_a()), "{}", text(&done));
        assert_eq!(rig.current("textpen"), None);
        // The right key is no harm where there is nothing to approve.
        let (done, _, _) = rig.install(&["textpen", "--trust", &key_a()]);
        assert_eq!(code(&done), 0, "{}", text(&done));

        // The same for an update of a program whose key is approved.
        rig.fake.catalog.borrow_mut().push("textpen-1.1.0");
        let (done, _, _) = rig.update(&["textpen", "--trust", &key_b()]);
        assert_eq!(code(&done), 77);
        assert_eq!(rig.current("textpen"), Some(v("1.0.0")));
        let (done, _, _) = rig.update(&["textpen", "--trust", &key_a()]);
        assert_eq!(code(&done), 0, "{}", text(&done));
        assert_eq!(rig.current("textpen"), Some(v("1.1.0")));
    }

    #[test]
    fn update_tells_the_app_the_key_it_was_given_to_approve_not_the_old_one() {
        let mut rig = Rig::trusting_a("trust-offer", &["textpen-1.0.0"]);
        assert_eq!(code(&rig.install(&["textpen"]).0), 0);
        rig.fake
            .catalog
            .borrow_mut()
            .push("textpen-1.3.0-other-key");
        rig.fake.told.borrow_mut().clear();
        let (done, ..) = rig.update(&["textpen", "--trust", &key_b()]);
        assert_eq!(code(&done), 0, "{}", text(&done));
        let Some(Package::Offer(offer)) = rig.fake.told().into_iter().next() else {
            panic!("an offer was asked");
        };
        assert_eq!(offer.key, Some(key_b()), "the key about to be approved");
        // Without it, the key kept for the program.
        let mut rig = Rig::trusting_a("trust-offer-old", &["textpen-1.0.0"]);
        assert_eq!(code(&rig.install(&["textpen"]).0), 0);
        rig.fake.told.borrow_mut().clear();
        let _ = rig.update(&["textpen"]);
        let Some(Package::Offer(offer)) = rig.fake.told().into_iter().next() else {
            panic!("an offer was asked");
        };
        assert_eq!(offer.key, Some(key_a()));
    }

    #[test]
    fn the_hint_for_a_held_program_says_the_key_must_be_known_to_be_right() {
        let mut rig = Rig::trusting_a("held-hint", &["textpen-1.0.0"]);
        assert_eq!(code(&rig.install(&["textpen"]).0), 0);
        rig.fake
            .catalog
            .borrow_mut()
            .push("textpen-1.3.0-other-key");
        let (_, out, _) = rig.update(&[]);
        assert!(out.contains("if the key is known to be right"), "{out}");
        assert!(out.contains(&format!("--trust {}", key_b())), "{out}");
    }

    #[test]
    fn a_key_is_not_an_option_and_is_given_once() {
        let mut rig = Rig::trusting_a("trust-words", &["textpen-1.0.0"]);
        for args in [
            &["textpen", "--trust", "--force"][..],
            &["textpen", "--trust", "a", "--trust", "b"][..],
        ] {
            for verb in ["install", "update"] {
                let (done, _, _) = if verb == "install" {
                    rig.install(args)
                } else {
                    rig.update(args)
                };
                assert_ne!(code(&done), 0, "{verb} {args:?}");
            }
        }
        assert!(
            rig.fake.told().is_empty(),
            "a mistake in the words asks nobody"
        );
    }

    #[test]
    fn a_new_key_stops_the_program_tells_the_app_once_and_waits_for_a_person() {
        let mut rig = Rig::trusting_a("stop", &["textpen-1.0.0"]);
        assert_eq!(code(&rig.install(&["textpen"]).0), 0);
        rig.fake
            .catalog
            .borrow_mut()
            .push("textpen-1.3.0-other-key");
        rig.fake.told.borrow_mut().clear();

        // Every program at once: nobody is asked, the program stays, the app is told.
        let (done, out, _) = rig.update(&[]);
        assert_eq!(code(&done), 0, "{}", text(&done));
        assert!(out.starts_with("textpen stays as it is"), "{out}");
        assert!(out.contains(&key_b()), "{out}");
        assert_eq!(rig.current("textpen"), Some(v("1.0.0")));
        assert_eq!(rig.store.stopped("textpen").unwrap(), Some(key_b()));
        assert!(rig.person.asked.is_empty());
        assert_eq!(
            rig.fake.told().last(),
            Some(&Package::KeyChanged(KeyChanged {
                name: "textpen".into(),
                key: key_b()
            }))
        );

        // The same key again is old news.
        rig.fake.told.borrow_mut().clear();
        let _ = rig.update(&[]);
        assert_eq!(rig.fake.told_verbs(), ["package offer"]);

        // A person at a terminal, naming the program: both keys are in the question.
        rig.person.terminal = true;
        rig.person.answers = vec![false];
        let (done, _, _) = rig.update(&["textpen"]);
        assert_eq!(code(&done), 77);
        assert_eq!(rig.current("textpen"), Some(v("1.0.0")));
        let question = &rig.person.asked[0];
        assert!(
            question.contains(&key_a()) && question.contains(&key_b()),
            "{question}"
        );

        rig.person.answers = vec![true];
        let (done, out, _) = rig.update(&["textpen"]);
        assert_eq!(code(&done), 0, "{}", text(&done));
        assert_eq!(out, "textpen 1.0.0 -> 1.3.0\n");
        assert_eq!(rig.store.key("textpen").unwrap(), Some(key_b()));
        assert_eq!(rig.store.stopped("textpen").unwrap(), None);

        // Naming the program with no terminal to ask at is a refusal, not a silent stop.
        let mut rig = Rig::trusting_a("stop2", &["textpen-1.0.0"]);
        assert_eq!(code(&rig.install(&["textpen"]).0), 0);
        rig.fake
            .catalog
            .borrow_mut()
            .push("textpen-1.3.0-other-key");
        let (done, _, _) = rig.update(&["textpen"]);
        assert_eq!(code(&done), 77);
        assert_eq!(rig.current("textpen"), Some(v("1.0.0")));
    }

    #[test]
    fn watch_tells_the_app_what_the_machine_has_and_then_looks_each_round() {
        let mut rig = Rig::trusting_a("watch", &["textpen-1.0.0"]);
        assert_eq!(code(&rig.install(&["textpen"]).0), 0);
        rig.store.mark_not_here(&["pickpen"]).unwrap();
        rig.fake.told.borrow_mut().clear();
        rig.fake.catalog.borrow_mut().push("textpen-1.1.0");

        assert_eq!(code(&rig.watch(2)), 0);
        // The state first; then the round (the offer and the file), then another round with
        // nothing newer.
        assert_eq!(
            rig.fake.told_verbs(),
            [
                "package state",
                "package offer",
                "package fetch",
                "package offer",
            ]
        );
        assert_eq!(
            rig.fake.told()[0],
            Package::State(State {
                target: TARGET.into(),
                installed: vec!["textpen".into()],
                not_here: vec!["pickpen".into()]
            })
        );
        assert_eq!(rig.current("textpen"), Some(v("1.1.0")));
    }

    #[test]
    fn watch_does_not_update_when_updating_is_off_but_still_tells_the_state() {
        let mut rig = Rig::trusting_a("auto", &["textpen-1.0.0"]);
        assert_eq!(code(&rig.install(&["textpen"]).0), 0);
        rig.fake.catalog.borrow_mut().push("textpen-1.1.0");
        rig.store.set_auto(false).unwrap();
        rig.fake.told.borrow_mut().clear();
        assert_eq!(code(&rig.watch(3)), 0);
        // No look at a source, but the state at the start and at each day that follows.
        assert_eq!(
            rig.fake.told_verbs(),
            ["package state", "package state", "package state"]
        );
        assert_eq!(rig.current("textpen"), Some(v("1.0.0")));
        // Turned on again, the next watch takes it.
        rig.store.set_auto(true).unwrap();
        assert_eq!(code(&rig.watch(1)), 0);
        assert_eq!(rig.current("textpen"), Some(v("1.1.0")));
    }

    #[test]
    fn watch_stops_a_program_for_a_key_once_however_many_rounds_pass() {
        let mut rig = Rig::trusting_a("watchkey", &["textpen-1.0.0"]);
        assert_eq!(code(&rig.install(&["textpen"]).0), 0);
        rig.fake
            .catalog
            .borrow_mut()
            .push("textpen-1.3.0-other-key");
        rig.fake.told.borrow_mut().clear();
        assert_eq!(code(&rig.watch(3)), 0);
        let told = rig.fake.told_verbs();
        assert_eq!(
            told.iter()
                .filter(|verb| **verb == "package key-changed")
                .count(),
            1
        );
        assert_eq!(
            told.iter().filter(|verb| **verb == "package offer").count(),
            3
        );
        assert_eq!(rig.current("textpen"), Some(v("1.0.0")));
        assert!(rig.person.asked.is_empty());
    }

    #[test]
    fn watch_ends_when_the_app_is_gone_and_not_when_one_program_cannot_be_looked_at() {
        let mut rig = Rig::trusting_a("gone", &["textpen-1.0.0"]);
        assert_eq!(code(&rig.install(&["textpen"]).0), 0);
        // The state is the first thing asked: no app, no watch.
        *rig.fake.refuses.borrow_mut() = Some(Asked::NoApp);
        let done = rig.watch(5);
        assert_eq!(code(&done), 69);
        assert_eq!(text(&done), "no bateri is connected to bring it");

        // The app goes after the state: the round ends the watch with the same status.
        let mut rounds = 0;
        let refuses = Rc::clone(&rig.fake.refuses);
        let (done, ..) = rig.run(|machine, _| {
            machine.watch(&mut || {
                rounds += 1;
                // The round after this one meets the app's absence and ends the watch; a machine
                // that does not look (updating off) would rest for ever, so this ends it first.
                assert!(rounds < 100, "the watch never met the app's absence");
                *refuses.borrow_mut() = Some(Asked::NoApp);
                true
            })
        });
        assert_eq!(code(&done), 69);
        assert_eq!(rounds, 1);

        // A trouble with one program (the source answers with a failure) is looked at again next round.
        let failed = Failure::new(Kind::Failed, "the source did not answer");
        let mut rounds = 0;
        let refuses = Rc::clone(&rig.fake.refuses);
        let (done, ..) = rig.run(|machine, _| {
            machine.watch(&mut || {
                rounds += 1;
                if rounds == 1 {
                    *refuses.borrow_mut() = Some(Asked::Reply(Reply::Failed(failed.clone())));
                }
                rounds < 3
            })
        });
        assert_eq!(code(&done), 0);
        assert_eq!(rounds, 3);
    }

    #[test]
    fn the_state_carries_no_more_names_than_a_request_may() {
        let mut rig = Rig::trusting_a("many", &[]);
        let names: Vec<String> = (0..NAMES_MAX + 5).map(|n| format!("p{n:02}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        rig.store.mark_not_here(&refs).unwrap();
        assert_eq!(code(&rig.watch(1)), 0);
        let Package::State(state) = &rig.fake.told()[0] else {
            panic!("not the state");
        };
        assert_eq!(state.not_here.len(), NAMES_MAX);
        // The request is one the reader reads back.
        let request = Request::Package(Package::State(state.clone()));
        assert!(Request::parse(&request.encode().unwrap()).is_ok());
    }
}
