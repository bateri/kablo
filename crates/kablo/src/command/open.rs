//! `split`, `tab` and `window`: start a terminal on this machine, or on a server (`-S`), and tell
//! the Mac where its pane goes.
//!
//! The terminal is the machine's own: `kablo new` starts it, it gets a number (`p9`) from the
//! counter, and the Mac only decides where a pane that shows it goes. What the machine is — the Mac
//! itself, or a server — is not something this knows: the Mac says it, in its answer. A terminal that
//! is a program is started right away and its pane asked for; one with no program (`split`, or a
//! shell named alone) is asked for first, as a pane of the Mac's own (`fresh`): the Mac makes it when
//! the asker is the Mac itself, and says `ok start` when the asker is a server, which then starts
//! the terminal with its bootstrap (`boot.sh`) and asks again. When no Mac is there to say, the
//! bootstrap's file is: a machine that has one is a server's.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use kablo_bateri::message::{Home, Placed, Reach, Reply, Request, WindowHome};
use kablo_bateri::{Anchor, Id, Landed, Side, Terminal};
use kablo_proto::SessionId;
use kablo_proto::v1::NewSession;

use super::state::State;
use super::words::{check_label, clean, is_machine, is_name, is_terminal};
use super::{Asked, Ctx, Done, Exit, Io, failure, finish, machine};
use crate::launch::{self, Failure};
use crate::{drive, sockdir};

/// What the terminal a command starts does not get from the one that asked: the tab it stands in
/// (that tab's pane is not the new terminal's), and what only the command reads.
const NOT_PASSED_ON: [&str; 4] = [
    "LC_BATERI_TAB_URL",
    "BATERI_TAB_URL",
    "TERM_SESSION_ID",
    "BATERI_QUIET_ORPHAN",
];

/// How many numbers are tried when a terminal made by hand holds the one that comes next.
const TRIES: u32 = 5;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Split,
    Tab,
    Window,
}

impl Mode {
    fn word(self) -> &'static str {
        match self {
            Mode::Split => "split",
            Mode::Tab => "tab",
            Mode::Window => "window",
        }
    }
}

/// What the words of the command ask for.
struct Wants {
    mode: Mode,
    anchor: Option<String>,
    side: Option<Side>,
    ratio: Option<u8>,
    cwd: Option<String>,
    keep: bool,
    wait: bool,
    focus: bool,
    json: bool,
    tab: Option<Id>,
    group: Option<Id>,
    window: Option<WindowHome>,
    new_group: Option<String>,
    label: Option<String>,
    server: Option<String>,
    program: Vec<String>,
}

impl Wants {
    /// A pane's label is kablo's own; a tab's is the Mac's.
    fn pane_label(&self) -> Option<&str> {
        match self.mode {
            Mode::Split => self.label.as_deref(),
            _ => None,
        }
    }

    fn tab_label(&self) -> Option<&str> {
        match self.mode {
            Mode::Split => None,
            _ => self.label.as_deref(),
        }
    }
}

/// Whether `word` names a server as `-S` takes it: as `ssh` is given it (`prod`, an alias or a
/// host of `~/.ssh/config`), starting with a letter or a digit so that it can never be read as an
/// option; or `@local`, this Mac. No `user@`: a pane of another machine is written with the host
/// alone (`p9@prod`).
fn is_server(word: &str) -> bool {
    word == "@local" || is_machine(word)
}

/// The value of an option that is written `--name VALUE` or `--name=VALUE`.
fn value(args: &[String], at: &mut usize, name: &str, missing: &str) -> Result<String, Exit> {
    let Some(arg) = args.get(*at) else {
        return Err(Exit::mistake(missing));
    };
    if let Some(value) = arg.strip_prefix(&format!("{name}=")) {
        return Ok(value.to_owned());
    }
    *at += 1;
    args.get(*at).cloned().ok_or_else(|| Exit::mistake(missing))
}

fn parse(mode: Mode, args: &[String]) -> Result<Wants, Exit> {
    let word = mode.word();
    let mut wants = Wants {
        mode,
        anchor: None,
        side: None,
        ratio: None,
        cwd: None,
        keep: false,
        wait: false,
        focus: false,
        json: false,
        tab: None,
        group: None,
        window: None,
        new_group: None,
        label: None,
        server: None,
        program: Vec::new(),
    };
    let mut at = 0;
    while let Some(arg) = args.get(at) {
        let name = arg.split('=').next().unwrap_or(arg);
        match arg.as_str() {
            "--" => {
                wants.program = args.get(at + 1..).unwrap_or_default().to_vec();
                break;
            }
            "-S" => {
                if wants.server.is_some() {
                    return Err(Exit::mistake("name one server"));
                }
                at += 1;
                let Some(server) = args.get(at) else {
                    return Err(Exit::mistake("-S takes a server"));
                };
                if server.starts_with('-') {
                    return Err(Exit::mistake(format!(
                        "a server's name does not start with a dash: '{}'",
                        clean(server)
                    )));
                }
                if !is_server(server) {
                    return Err(Exit::mistake(format!(
                        "-S takes a server as ssh does ('prod', an alias or a host of ~/.ssh/config, with no user@) or @local, not '{}'",
                        clean(server)
                    )));
                }
                wants.server = Some(server.clone());
            }
            "--left" | "--right" | "--above" | "--below" => {
                if mode != Mode::Split {
                    return Err(Exit::mistake(format!("{word} takes no side")));
                }
                if wants.side.is_some() {
                    return Err(Exit::mistake("name one side"));
                }
                wants.side = Side::from_word(arg.trim_start_matches("--"));
            }
            _ if name == "--ratio" => {
                if mode != Mode::Split {
                    return Err(Exit::mistake(format!("{word} takes no ratio")));
                }
                let text = value(args, &mut at, "--ratio", "--ratio takes a percent")?;
                wants.ratio = match text.parse::<u8>() {
                    Ok(percent) if (1..=99).contains(&percent) && percent.to_string() == text => {
                        Some(percent)
                    }
                    _ => return Err(Exit::mistake("--ratio is a percent from 1 to 99")),
                };
            }
            _ if name == "--cwd" => {
                let text = value(args, &mut at, "--cwd", "--cwd takes a directory")?;
                if text.is_empty() {
                    return Err(Exit::mistake("--cwd takes a directory"));
                }
                wants.cwd = Some(text);
            }
            _ if name == "--tab" => {
                if mode != Mode::Split {
                    return Err(Exit::mistake(format!(
                        "{word} takes no --tab: a pane goes into a tab (split --tab t3)"
                    )));
                }
                if wants.tab.is_some() {
                    return Err(Exit::mistake("name one tab"));
                }
                let text = value(args, &mut at, "--tab", "--tab takes a tab")?;
                super::words::check_id("split", "tab", &text, |_| Exit::status(64))?;
                wants.tab = Id::parse(&text);
            }
            _ if name == "--group" => {
                if mode != Mode::Tab {
                    return Err(Exit::mistake(format!(
                        "{word} takes no --group: a tab goes into a group (tab --group g1)"
                    )));
                }
                if wants.group.is_some() {
                    return Err(Exit::mistake("name one group"));
                }
                let text = value(args, &mut at, "--group", "--group takes a group")?;
                super::words::check_id("tab", "group", &text, |_| Exit::status(64))?;
                wants.group = Id::parse(&text);
            }
            _ if name == "--window" => {
                if mode != Mode::Tab {
                    return Err(Exit::mistake(format!(
                        "{word} takes no --window: a tab goes into a window (tab --window w1)"
                    )));
                }
                if wants.window.is_some() {
                    return Err(Exit::mistake("name one window"));
                }
                let text = value(args, &mut at, "--window", "--window takes a window")?;
                super::words::check_id("tab", "window", &text, |_| Exit::status(64))?;
                wants.window = Id::parse(&text).map(WindowHome::Existing);
            }
            _ if name == "--new-group" => {
                if mode != Mode::Tab {
                    return Err(Exit::mistake(format!(
                        "{word} takes no --new-group: a tab goes into a group made for it (tab --new-group api)"
                    )));
                }
                if wants.new_group.is_some() {
                    return Err(Exit::mistake("name one new group"));
                }
                let text = value(
                    args,
                    &mut at,
                    "--new-group",
                    "--new-group takes the name of the group",
                )?;
                check_label("--new-group", &text)?;
                wants.new_group = Some(text);
            }
            _ if name == "--label" => {
                if mode == Mode::Window {
                    return Err(Exit::mistake(
                        "window takes no --label: a window has no label; rename tab names the tab in it",
                    ));
                }
                if wants.label.is_some() {
                    return Err(Exit::mistake("name one label"));
                }
                let text = value(args, &mut at, "--label", "--label takes a text")?;
                check_label("--label", &text)?;
                wants.label = Some(text);
            }
            "--keep-open" => wants.keep = true,
            "--focus" => wants.focus = true,
            "--wait" => wants.wait = true,
            "--json" => wants.json = true,
            _ if arg.starts_with('-') => {
                return Err(Exit::mistake(format!(
                    "{word} takes no option '{}'",
                    clean(arg)
                )));
            }
            _ => {
                if mode == Mode::Window {
                    return Err(Exit::mistake(
                        "window goes beside no pane; a program goes after --",
                    ));
                }
                if wants.anchor.is_some() {
                    return Err(Exit::mistake("the program goes after --"));
                }
                wants.anchor = Some(arg.clone());
            }
        }
        at += 1;
    }
    if mode != Mode::Split {
        wants.side = Some(Side::Tab);
    }
    if mode == Mode::Window {
        wants.window = Some(WindowHome::New);
    }
    // What does not go together is a mistake here, before anything is started (the Mac reads the
    // same rules).
    if wants.tab.is_some() && wants.anchor.is_some() {
        return Err(Exit::mistake(
            "split goes beside a pane or into a tab (--tab), not both",
        ));
    }
    if wants.group.is_some() && (wants.window.is_some() || wants.new_group.is_some()) {
        return Err(Exit::mistake(
            "--group is a group that is in its window already: it goes with no --window and no --new-group",
        ));
    }
    if wants.anchor.is_some() && (wants.group.is_some() || wants.window.is_some()) {
        return Err(Exit::mistake(
            "tab goes beside a pane or into a group or a window, not both",
        ));
    }
    Ok(wants)
}

/// `split`, `tab` or `window` with `args`.
pub fn open(ctx: &Ctx, io: &mut Io<'_>, mode: Mode, args: &[String]) -> Done {
    let wants = parse(mode, args)?;
    let me = ctx.here();
    // The pane goes beside the one named or, with none named, the one this runs in, whatever that
    // is called. What the agent writes is a number: a name that is no number is no target, and most
    // often it is the program, written where a pane goes. Run in no terminal (cron) with none named
    // there is nothing to go beside: the terminal starts and waits in the Mac's list of terminals
    // running in the background, which is where a terminal no pane shows goes.
    let anchor = match (&wants.anchor, &me) {
        (Some(named), _) => {
            if !is_terminal(named) {
                return Err(super::verbs::not_a_terminal(
                    io,
                    named,
                    "; a program goes after --",
                ));
            }
            Some(named.clone())
        }
        (None, Some(me)) => Some(me.clone()),
        (None, None) => None,
    };
    let job = Job {
        ctx,
        wants,
        me,
        anchor,
        state: ctx.state(),
    };
    job.run(io)
}

/// An opening in the making.
struct Job<'a> {
    ctx: &'a Ctx,
    wants: Wants,
    /// The terminal this runs in, under whatever name it has.
    me: Option<String>,
    /// The terminal the pane goes beside; none is a terminal no pane shows.
    anchor: Option<String>,
    state: State,
}

/// What is known of the pane when the answer is written.
struct Landing {
    /// The terminal as it is written: `p9`, or `p9@prod`.
    id: String,
    landed: Landed,
    mac_waits: bool,
    next: Option<String>,
    squeezed: bool,
    hidden: bool,
    tab: Option<Id>,
    group: Option<Id>,
    window: Option<Id>,
    kept: bool,
}

impl Landing {
    fn new(id: &str) -> Landing {
        Landing {
            id: id.to_owned(),
            landed: Landed::Waiting,
            mac_waits: false,
            next: None,
            squeezed: false,
            hidden: false,
            tab: None,
            group: None,
            window: None,
            kept: false,
        }
    }

    /// What the Mac said of a pane that was placed.
    fn placed(&mut self, placed: &Placed) {
        self.landed = placed.landed;
        // The Mac has it: it waits for its anchor to be on screen.
        self.mac_waits = placed.landed == Landed::Waiting;
        self.next = placed
            .next_to
            .as_ref()
            .map(ToString::to_string)
            .filter(|name| is_name(name));
        self.squeezed = placed.squeezed;
        self.hidden = placed.hidden;
        self.tab = placed.tab;
        self.group = placed.group;
        self.window = placed.window;
        self.kept = placed.keyboard_stayed;
    }
}

/// How the start of a terminal ended.
enum Started {
    Ready,
    /// A terminal of that name is there already (one made by hand holds the number).
    Taken,
    Failed(String),
}

impl Job<'_> {
    fn run(self, io: &mut Io<'_>) -> Done {
        // On a server (`-S`) the directory is the server's to say, and the server says if it is none.
        if let Some(server) = self.wants.server.clone() {
            if let Some(cwd) = &self.wants.cwd {
                if clean(cwd) != *cwd {
                    return Err(Exit::mistake(
                        "the directory has a control character in its name",
                    ));
                }
            }
            match machine::reach(self.ctx, &server)? {
                // This machine is the one named: the plain terminal, and the plain pane.
                Reach::Here => {}
                way => return self.on_server(io, &server, &way),
            }
        }
        self.here(io)
    }

    // ─── on this machine ─────────────────────────────────────────────────

    /// Where it starts: the caller's directory unless `--cwd` names one.
    fn directory(&self) -> Result<String, Exit> {
        let home = self.ctx.home();
        let given = match self.wants.cwd.as_deref() {
            Some("~") => home.display().to_string(),
            Some(text) => match text.strip_prefix("~/") {
                Some(rest) => home.join(rest).display().to_string(),
                None => text.to_owned(),
            },
            None => {
                let pwd = std::env::var_os("PWD")
                    .map(PathBuf::from)
                    .filter(|path| path.is_absolute() && path.is_dir())
                    .or_else(|| std::env::current_dir().ok().filter(|path| path.is_dir()));
                pwd.unwrap_or(home).display().to_string()
            }
        };
        if !Path::new(&given).is_dir() {
            return Err(Exit::failed(format!(
                "'{}' is not a directory",
                clean(&given)
            )));
        }
        Ok(given)
    }

    fn here(&self, io: &mut Io<'_>) -> Done {
        let cwd = self.directory()?;
        let wants = &self.wants;
        // A shell bateri has an integration for, with nothing to run: a shell to work in, the same
        // as no program, but that one.
        let shell = work_shell(&wants.program);
        // Whether the Mac can be asked who starts it: there is a pane to go beside, and nothing to
        // wait for (a pane the Mac makes is no terminal to wait on).
        let askable = self.anchor.is_some() && !wants.wait;
        let boot = self.state.boot().is_file();
        let mut program = wants.program.clone();
        let mut shell_asked = None;
        if let Some(path) = shell {
            // One that waits or has no pane to go beside is a program like any, where there is no
            // bootstrap to start it with.
            if askable || boot {
                program.clear();
                shell_asked = Some(path);
            }
        }
        if program.is_empty() {
            if askable {
                return self.ask_first(io, &cwd, shell_asked.as_deref());
            }
            if !boot {
                if wants.wait {
                    return Err(Exit::mistake(
                        "--wait needs a program here: name one after --",
                    ));
                }
                return Err(Exit::failed(
                    "a pane with no program goes beside a pane: run this in a bateri terminal, or name one",
                ));
            }
            program = self.bootstrap();
        } else if wants.keep {
            // No Mac pipe holds this terminal after its program, on any machine: the terminal itself
            // carries on with the user's login shell.
            let mut wrapped = vec![
                "sh".to_owned(),
                "-c".to_owned(),
                "\"$@\"; exec \"${SHELL:-/bin/sh}\" -l".to_owned(),
                "sh".to_owned(),
            ];
            wrapped.append(&mut program);
            program = wrapped;
        }

        // The number, and the terminal. A hand-made terminal can hold the number
        // (`kablo new --id t5`): the next is tried.
        let mut tries = 0;
        let id = loop {
            let id = self.number(io)?;
            match self.start(&id, &cwd, shell_asked.as_deref(), &program) {
                Started::Ready => break id,
                Started::Taken if tries + 1 < TRIES => tries += 1,
                Started::Taken => {
                    return Err(Exit::failed(format!(
                        "kablo could not start {id}: a session called {id} exists already"
                    )));
                }
                Started::Failed(text) => {
                    return Err(Exit::failed(format!(
                        "kablo could not start {id}: {}",
                        clean(&text)
                    )));
                }
            }
        };
        self.state.record(&id, self.me.as_deref(), self.wants.keep);
        let mut landing = Landing::new(&id);
        if self.anchor.is_some() {
            let place = self.place(&id, None, None, None);
            self.place_it(io, &mut landing, &place, &id, false)?;
        }
        self.report(io, &landing);
        if self.wants.wait {
            return finish(drive::wait(&[OsString::from(&id)], io.err));
        }
        Ok(())
    }

    /// A terminal with no program, or a shell named alone: the Mac is asked for a pane of its own
    /// first (`fresh`), and says who starts it.
    fn ask_first(&self, io: &mut Io<'_>, cwd: &str, shell: Option<&str>) -> Done {
        if clean(cwd) != cwd {
            return Err(Exit::mistake(
                "the directory has a control character in its name",
            ));
        }
        let fresh = std::path::absolute(cwd)
            .map(|path| path.display().to_string())
            .unwrap_or_else(|_| cwd.to_owned());
        let id = self.number(io)?;
        self.state.record(&id, self.me.as_deref(), self.wants.keep);
        let mut landing = Landing::new(&id);
        let place = self.place(&id, Some(fresh), shell.map(str::to_owned), None);
        match self.ctx.ask(&Request::Place(place.clone()))? {
            Asked::Reply(Reply::Placed(placed)) => landing.placed(&placed),
            // The asker is a server: the terminal is its own to start, with the bootstrap, and the
            // Mac is asked again for the pane that shows it.
            Asked::Reply(Reply::Start) => {
                if let Err(exit) = self.start_bootstrap(&id, cwd, shell) {
                    self.state.forget(&id);
                    return Err(exit);
                }
                let place = self.place(&id, None, None, None);
                self.place_it(io, &mut landing, &place, &id, false)?;
            }
            Asked::Reply(Reply::Failed(failed)) => {
                self.state.forget(&id);
                return Err(failure(&failed));
            }
            // No Mac to say: a machine with a bootstrap is a server's, and starts it; one without
            // has nobody to make its pane.
            Asked::NoMac => {
                if !self.state.boot().is_file() {
                    self.state.forget(&id);
                    return Err(Exit::new(
                        69,
                        "no Mac is connected: it is the Mac that makes a pane with no program",
                    ));
                }
                self.start_bootstrap(&id, cwd, shell)?;
                let place = self.place(&id, None, None, None);
                self.wait_note(io, &mut landing, &place);
            }
            other => {
                io.say(match other {
                    Asked::Silent => {
                        "the Mac did not answer; its pane will be placed when it next connects"
                    }
                    _ => {
                        "the Mac answered with something this does not read; its pane will be placed when it next connects"
                    }
                });
                if self.state.boot().is_file() {
                    self.start_bootstrap(&id, cwd, shell)?;
                    let place = self.place(&id, None, None, None);
                    self.wait_note(io, &mut landing, &place);
                } else {
                    self.wait_note(io, &mut landing, &place);
                }
            }
        }
        self.report(io, &landing);
        Ok(())
    }

    /// The program of a terminal that is a server's shell: its bootstrap, run from the file, with no
    /// parent block, nonce or tab.
    fn bootstrap(&self) -> Vec<String> {
        let version = std::env::var("LC_TERMINAL_VERSION").unwrap_or_default();
        let version = if version
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'+' | b'-'))
        {
            version
        } else {
            String::new()
        };
        vec![
            "sh".to_owned(),
            self.state.boot().display().to_string(),
            String::new(),
            String::new(),
            version,
            "-".to_owned(),
        ]
    }

    fn start_bootstrap(&self, id: &str, cwd: &str, shell: Option<&str>) -> Result<(), Exit> {
        match self.start(id, cwd, shell, &self.bootstrap()) {
            Started::Ready => Ok(()),
            Started::Taken => Err(Exit::failed(format!(
                "kablo could not start {id}: a session called {id} exists already"
            ))),
            Started::Failed(text) => Err(Exit::failed(format!(
                "kablo could not start {id}: {}",
                clean(&text)
            ))),
        }
    }

    /// The number of the next terminal, as `pN`.
    fn number(&self, _io: &mut Io<'_>) -> Result<String, Exit> {
        self.state
            .next_number()
            .map(|number| Terminal(number).to_string())
            .ok_or_else(|| {
                Exit::failed(format!(
                    "cannot number a pane: {} is not writable",
                    self.state.dir().display()
                ))
            })
    }

    /// Starts terminal `id`, in `cwd`, running `program`. It gets the umask and the environment of
    /// the caller (this is no subshell: neither was changed) but for the tab the caller stands in,
    /// the `PATH` with the command on it, and the shell it was asked for.
    fn start(&self, id: &str, cwd: &str, shell: Option<&str>, program: &[String]) -> Started {
        let dir = match sockdir::socket_dir() {
            Ok(dir) => dir,
            Err(error) => return Started::Failed(error.to_string()),
        };
        let Ok(name) = SessionId::new(id) else {
            return Started::Failed(format!("{id} is no name for a session"));
        };
        let mut request = NewSession::default();
        request.argv = program
            .iter()
            .map(|word| word.as_bytes().to_vec())
            .collect();
        request.label = self.wants.pane_label().map(str::to_owned);
        let absolute = std::path::absolute(cwd).unwrap_or_else(|_| PathBuf::from(cwd));
        request.cwd = Some(absolute.as_os_str().as_encoded_bytes().to_vec());
        request
            .env
            .push((b"PATH".to_vec(), self.path().into_bytes()));
        if let Some(shell) = shell {
            request
                .env
                .push((b"SHELL".to_vec(), shell.as_bytes().to_vec()));
        }
        // The dock's tier (the Mac's `"blocks"`) goes on to the new terminal: its shell reads it.
        if std::env::var("BATERI_DOCK").is_ok_and(|tier| tier == "off") {
            request.env.push((b"BATERI_DOCK".to_vec(), b"off".to_vec()));
        }
        match launch::launch_without(&dir, &name, &request, &NOT_PASSED_ON) {
            Ok(()) => Started::Ready,
            Err(Failure::Exists(_)) => Started::Taken,
            Err(other) => Started::Failed(other.to_string()),
        }
    }

    /// The `PATH` of the terminal: the caller's, with the directory of the command on it, once. A
    /// program there calls `bateri` all the same, and the window's relay runs the command by its
    /// path from a login shell that never had it.
    fn path(&self) -> String {
        let path = std::env::var("PATH").unwrap_or_default();
        let bin = self.state.bin_dir();
        if !bin.join("bateri").is_file() {
            return path;
        }
        let bin = bin.display().to_string();
        if format!(":{path}:").contains(&format!(":{bin}:")) {
            path
        } else if path.is_empty() {
            bin
        } else {
            format!("{bin}:{path}")
        }
    }

    // ─── the pane ────────────────────────────────────────────────────────

    /// The `place` message: the pane of terminal `id`. The anchor is always written out: a message
    /// that waits is read when no conversation names its sender.
    fn place(
        &self,
        id: &str,
        fresh: Option<String>,
        shell: Option<String>,
        server: Option<String>,
    ) -> kablo_bateri::message::Place {
        let wants = &self.wants;
        kablo_bateri::message::Place {
            terminal: Terminal::parse(id).unwrap_or(Terminal(0)),
            anchor: self.anchor.as_deref().and_then(Anchor::parse),
            side: wants.side,
            ratio: wants.ratio,
            keep_open: wants.keep,
            focus: wants.focus,
            home: Home {
                tab: wants.tab,
                group: wants.group,
                new_group: wants.new_group.clone(),
                window: wants.window.clone(),
                label: wants.tab_label().map(str::to_owned),
            },
            fresh,
            shell,
            server,
        }
    }

    /// Asks the Mac for the pane and tells what it said; one that refuses leaves no terminal running.
    fn place_it(
        &self,
        io: &mut Io<'_>,
        landing: &mut Landing,
        place: &kablo_bateri::message::Place,
        id: &str,
        remote: bool,
    ) -> Done {
        match self.ctx.ask(&Request::Place(place.clone()))? {
            Asked::Reply(Reply::Placed(placed)) => {
                landing.placed(&placed);
                return Ok(());
            }
            Asked::Reply(Reply::Failed(failed)) => {
                // The Mac refused: nothing is left running for a request that failed. (On a
                // server it is the server's terminal, which the caller ends.)
                if !remote {
                    let _ = drive::end(
                        &[OsString::from(id), OsString::from("--force")],
                        &mut std::io::sink(),
                    );
                    self.state.forget(id);
                }
                return Err(failure(&failed));
            }
            Asked::NoMac => {}
            Asked::Silent => {
                io.say("the Mac did not answer; its pane will be placed when it next connects");
            }
            _ => io.say(
                "the Mac answered with something this does not read; its pane will be placed when it next connects",
            ),
        }
        if remote {
            // The terminal runs on the server and is in its list; the Mac has no pane for it yet.
            landing.landed = Landed::Waiting;
            landing.mac_waits = true;
        } else {
            self.wait_note(io, landing, place);
        }
        Ok(())
    }

    /// No Mac to place it: the message waits on this machine.
    fn wait_note(
        &self,
        io: &mut Io<'_>,
        landing: &mut Landing,
        place: &kablo_bateri::message::Place,
    ) {
        landing.landed = Landed::Waiting;
        let id = landing.id.clone();
        let written = Request::Place(place.clone())
            .encode()
            .is_ok_and(|text| self.state.note(&id, &text));
        if !written {
            io.say(&format!(
                "{id} could not be put in the line for a Mac ({} is not writable); it runs, and the Mac lists it",
                self.state.dir().display()
            ));
        }
    }

    /// The answer: the number, or the object (every key always there, `null` where the Mac has not
    /// said: the pane waits for a window, or its tab is in no group); and one line when the pane is
    /// not where it was asked to be.
    fn report(&self, io: &mut Io<'_>, landing: &Landing) {
        let id = &landing.id;
        if self.wants.json {
            let named =
                |id: Option<Id>| id.map_or_else(|| "null".to_owned(), |id| format!("\"{id}\""));
            let next = landing
                .next
                .as_ref()
                .map_or_else(|| "null".to_owned(), |name| format!("\"{name}\""));
            let _ = writeln!(
                io.out,
                "{{\"pane\":\"{id}\",\"tab\":{},\"group\":{},\"window\":{},\"landed\":\"{}\",\"next_to\":{next},\"squeezed\":{},\"hidden\":{}}}",
                named(landing.tab),
                named(landing.group),
                named(landing.window),
                landing.landed.word(),
                landing.squeezed,
                landing.hidden,
            );
        } else {
            let _ = writeln!(io.out, "{id}");
        }
        // The anchor as a line says it: its number, or "this terminal" for the caller's own when
        // that has none.
        let said = match &self.anchor {
            Some(anchor) if !is_terminal(anchor) && self.me.as_deref() == Some(anchor.as_str()) => {
                "this terminal".to_owned()
            }
            Some(anchor) => anchor.clone(),
            None => String::new(),
        };
        match landing.landed {
            Landed::Waiting => {
                if self.anchor.is_none() {
                    // A server's command that `-S` started is told not to say it: the caller says it
                    // once, for the address it gives.
                    if std::env::var_os("BATERI_QUIET_ORPHAN").is_none() {
                        io.say(&format!(
                            "{id} waits in the Mac's list of terminals running in the background: this ran in no terminal, so it has no pane to go beside"
                        ));
                    }
                } else if landing.mac_waits && landing.next.is_none() {
                    // The Mac kept nothing to place it by: it could not make the pane just now.
                    io.say(&format!(
                        "{id} waits in the Mac's list of terminals running in the background: the Mac could not make its pane now"
                    ));
                } else if landing.mac_waits {
                    io.say(&format!(
                        "{id} waits in the Mac's list of terminals running in the background, until {said} is on screen"
                    ));
                } else {
                    io.say(&format!("{id} will take its place when a Mac connects"));
                }
            }
            Landed::NewTab => io.say(&format!(
                "{id} is in a tab of its own: there was no room beside {said}"
            )),
            Landed::BesideGroup => io.say(&format!(
                "{id} is beside the group of {said}: {said} is too small to split"
            )),
            Landed::AsAsked => {}
        }
        if landing.hidden {
            io.say(&format!("{id} is behind a pane that fills its tab"));
        }
        if landing.kept {
            io.say(&format!(
                "{id} did not take the keyboard: the cursor is in another pane of that window; bateri notify asks the user to look"
            ));
        }
    }

    // ─── on another machine ──────────────────────────────────────────────

    /// The terminal is started on `server` by its own `bateri split`, run in no terminal there,
    /// which numbers it and says the number; then the Mac is asked for the pane, beside this
    /// machine's own.
    fn on_server(&self, io: &mut Io<'_>, server: &str, way: &Reach) -> Done {
        let wants = &self.wants;
        let mut words = vec!["split".to_owned()];
        if let Some(cwd) = &wants.cwd {
            words.push("--cwd".to_owned());
            words.push(cwd.clone());
        }
        if let Some(label) = wants.pane_label() {
            words.push("--label".to_owned());
            words.push(label.to_owned());
        }
        if wants.keep {
            words.push("--keep-open".to_owned());
        }
        words.push("--".to_owned());
        words.extend(wants.program.iter().cloned());
        let ran = machine::remote(
            self.ctx,
            io,
            way,
            server,
            &words,
            machine::Output::Capture,
            false,
        )?;
        if ran.status != 0 {
            if ran.status == 255 {
                io.say(&format!("could not reach {server} over ssh"));
            }
            return Err(Exit::status(if ran.status == 127 { 1 } else { ran.status }));
        }
        // A login shell may print lines of its own before: the number is the last line.
        let out = ran.stdout.trim_end_matches('\n');
        let number = out.rsplit('\n').next().unwrap_or_default();
        if !is_terminal(number) {
            return Err(Exit::failed(format!(
                "{server} did not say which terminal it started"
            )));
        }
        let machine_name = server.rsplit('@').next().unwrap_or(server);
        let id = format!("{number}@{machine_name}");
        let mut landing = Landing::new(&id);
        if self.anchor.is_some() {
            let place = self.place(number, None, None, Some(server.to_owned()));
            if let Err(exit) = self.place_it(io, &mut landing, &place, number, true) {
                // In a subshell's silence: the failure the person is told is the Mac's.
                self.end_over(io, server, way, number);
                return Err(exit);
            }
        }
        self.report(io, &landing);
        if wants.wait {
            let ran = machine::remote(
                self.ctx,
                io,
                way,
                server,
                &["wait".to_owned(), number.to_owned()],
                machine::Output::Print,
                false,
            )?;
            return finish(ran.status);
        }
        Ok(())
    }

    /// Ends the terminal that was started on `server`, quietly: the Mac refused its pane.
    fn end_over(&self, io: &mut Io<'_>, server: &str, way: &Reach, number: &str) {
        let words = [
            "close".to_owned(),
            "pane".to_owned(),
            number.to_owned(),
            "--force".to_owned(),
        ];
        let _ = machine::remote(
            self.ctx,
            io,
            way,
            server,
            &words,
            machine::Output::Quiet,
            false,
        );
    }
}

/// Whether the program is a shell bateri has an integration for with nothing to run but itself
/// (`zsh`, `/opt/homebrew/bin/fish`, `bash -l`): the path of the shell. Any other (`sh`,
/// `zsh -c make`, one that is not installed) is a program like any.
fn work_shell(program: &[String]) -> Option<String> {
    let first = program.first()?;
    kablo_bateri::ShellKind::of_name(first.rsplit('/').next().unwrap_or(first))?;
    match program.get(1..)? {
        [] => {}
        [flag] if matches!(flag.as_str(), "-l" | "-i" | "-il" | "-li" | "--login") => {}
        _ => return None,
    }
    let found = find_program(first)?;
    // A path with a control character in it is no path to hand to a window.
    (found.starts_with('/') && clean(&found) == found).then_some(found)
}

/// Where `name` is: as the shell finds a program, along `PATH`; a name with a slash is the file.
fn find_program(name: &str) -> Option<String> {
    use std::os::unix::fs::PermissionsExt;
    let runs = |path: &Path| {
        path.metadata()
            .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
    };
    if name.contains('/') {
        return runs(Path::new(name)).then(|| name.to_owned());
    }
    let path = std::env::var("PATH").ok()?;
    path.split(':')
        .filter(|dir| !dir.is_empty())
        .map(|dir| Path::new(dir).join(name))
        .find(|candidate| runs(candidate))
        .map(|found| found.display().to_string())
}
