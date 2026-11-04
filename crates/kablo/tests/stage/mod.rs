//! The `bateri` command run for real: the real `kablo` binary, in a temporary home and socket
//! directory, against a test in the place of the Mac (a connection to `kablo attach`, as the Mac
//! is a program with one, the method of `app.rs`). What the command says to the Mac is read back
//! and compared with `kablo-bateri`'s own reading, and what the Mac says back is whatever the test
//! says it does, so the command and the window cannot drift apart without a test saying so.

// Test support: a failure here is a failed test.
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::common::{Bridge, Env};

/// What a run of the command said.
#[derive(Debug)]
pub struct Said {
    pub status: i32,
    pub out: String,
    pub err: String,
}

/// How long a run may take before the test gives up on it.
const LIMIT: Duration = Duration::from_secs(60);

/// One run: where it is, and what to do about the conversations it has.
pub struct Run<'a> {
    pub here: Option<&'a str>,
    pub args: &'a [&'a str],
    pub input: Option<&'a [u8]>,
    /// The Mac's answer to the conversation of this turn (from 0), else the stage's.
    pub turns: &'a [&'a str],
    /// Variables of the command.
    pub vars: &'a [(&'a str, &'a str)],
}

pub struct Stage {
    pub env: Env,
    pub home: PathBuf,
    app: Option<Bridge>,
    served: usize,
    /// Every message the Mac was told, in order.
    pub told: Vec<String>,
    reply: Option<String>,
    runs: usize,
    runners: Vec<String>,
    /// The next run ends the terminal it is run in: what is waited for is that.
    ending: bool,
}

impl Stage {
    /// A stage with no Mac connected.
    pub fn new() -> Stage {
        let env = Env::new();
        let home = env.dir.join("home");
        std::fs::create_dir(&home).unwrap();
        Stage {
            env,
            home,
            app: None,
            served: 0,
            told: Vec::new(),
            reply: None,
            runs: 0,
            runners: Vec::new(),
            ending: false,
        }
    }

    /// A stage whose Mac says `reply` to whatever it is asked.
    pub fn with_mac(reply: &str) -> Stage {
        let mut stage = Stage::new();
        stage.connect_mac();
        stage.mac_says(reply);
        stage
    }

    /// The Mac connects.
    pub fn connect_mac(&mut self) {
        if self.app.is_none() {
            let mut app = Bridge::start(&self.env);
            app.take_tickets();
            self.app = Some(app);
        }
    }

    /// The Mac goes away.
    pub fn no_mac(&mut self) {
        self.app = None;
        self.served = 0;
    }

    /// From now on the Mac says `reply` to each conversation.
    pub fn mac_says(&mut self, reply: &str) {
        self.connect_mac();
        self.reply = Some(reply.to_owned());
    }

    /// The state the command keeps: `~/.local/share/bateri`.
    pub fn state(&self) -> PathBuf {
        self.home.join(".local/share/bateri")
    }

    /// Forgets what the Mac was told so far.
    pub fn forget_told(&mut self) {
        self.told.clear();
    }

    /// A terminal of this machine under `id`, which runs `cat`: what is typed to it is echoed, so
    /// `capture` shows it.
    pub fn terminal(&mut self, id: &str) {
        self.terminal_running(id, &["/bin/sh", "-c", "cat"]);
    }

    /// A terminal of this machine under `id`, running a program.
    pub fn terminal_running(&mut self, id: &str, program: &[&str]) {
        let output = self
            .env
            .kablo()
            .args(["new", "--id", id, "--"])
            .args(program)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "kablo new {id}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        self.env.track_named_alike(id);
    }

    /// `bateri ARGS` run in no terminal.
    pub fn run(&mut self, args: &[&str]) -> Said {
        self.go(&Run {
            here: None,
            args,
            input: None,
            turns: &[],
            vars: &[],
        })
    }

    /// `bateri ARGS` run in the terminal `here`.
    pub fn run_in(&mut self, here: &str, args: &[&str]) -> Said {
        self.go(&Run {
            here: Some(here),
            args,
            input: None,
            turns: &[],
            vars: &[],
        })
    }

    /// `bateri ARGS` run in the terminal `here`, which it ends: nothing is left to say a status, so
    /// the run is over when the terminal is.
    pub fn run_ending_in(&mut self, here: &str, args: &[&str]) {
        self.ending = true;
        self.run_in(here, args);
    }

    /// The terminal `here`, which runs what is put in its directory of jobs one after another: a
    /// command run by it is run in that terminal, which is what `kablo here` answers from. Returns
    /// the directory.
    fn runner(&mut self, here: &str) -> PathBuf {
        let jobs = self.env.dir.join(format!("jobs-{here}"));
        if self.runners.iter().any(|name| name == here) {
            return jobs;
        }
        std::fs::create_dir(&jobs).unwrap();
        let script = self.env.script(
            &format!("runner-{here}.sh"),
            "#!/bin/sh\ntrap '' INT\ncd \"$1\" || exit 1\nwhile :; do\n  for f in job.*.sh; do\n    [ -f \"$f\" ] || continue\n    mv \"$f\" \"running.$f\"\n    sh \"running.$f\"\n    rm -f \"running.$f\"\n  done\n  sleep 0.02\ndone\n",
        );
        let kablo = env!("CARGO_BIN_EXE_kablo");
        let started = self
            .env
            .kablo()
            .args(["new", "--id", here])
            .args(["--env", &format!("HOME={}", self.home.display())])
            .args(["--env", &format!("KABLO={kablo}")])
            .args(["--env", &format!("KABLO_DIR={}", self.env.dir.display())])
            .arg("--")
            .arg("/bin/sh")
            .arg(&script)
            .arg(&jobs)
            .output()
            .unwrap();
        assert!(
            started.status.success(),
            "a terminal {here} to run in: {}",
            String::from_utf8_lossy(&started.stderr)
        );
        self.env.track_named_alike(here);
        self.runners.push(here.to_owned());
        jobs
    }

    /// A run with its own turns of the Mac's.
    pub fn go(&mut self, run: &Run<'_>) -> Said {
        self.runs += 1;
        let work = self.env.dir.join(format!("run{}", self.runs));
        std::fs::create_dir(&work).unwrap();
        let (out, err, code) = (work.join("out"), work.join("err"), work.join("code"));
        let input = work.join("in");
        std::fs::write(&input, run.input.unwrap_or_default()).unwrap();
        let kablo = env!("CARGO_BIN_EXE_kablo");
        let mut waiting = match run.here {
            None => {
                let mut command = Command::new(kablo);
                command
                    .arg("bateri")
                    .args(run.args)
                    .env("KABLO_DIR", &self.env.dir)
                    .env("HOME", &self.home)
                    .envs(run.vars.iter().copied())
                    .stdin(Stdio::from(File::open(&input).unwrap()))
                    .stdout(Stdio::from(File::create(&out).unwrap()))
                    .stderr(Stdio::from(File::create(&err).unwrap()));
                Waiting::Child(command.spawn().unwrap())
            }
            Some(here) => {
                let jobs = self.runner(here);
                let mut job = String::from("\"$KABLO\" bateri");
                for arg in run.args {
                    job.push_str(&format!(" '{}'", arg.replace('\'', "'\\''")));
                }
                for (name, value) in run.vars {
                    job = format!("{name}='{value}' {job}");
                }
                job.push_str(&format!(
                    " < '{}' > '{}' 2> '{}'\necho $? > '{}.tmp'\nmv '{}.tmp' '{}'\n",
                    input.display(),
                    out.display(),
                    err.display(),
                    code.display(),
                    code.display(),
                    code.display()
                ));
                let name = format!("job.{}.sh", self.runs);
                std::fs::write(jobs.join(format!("{name}.tmp")), job).unwrap();
                std::fs::rename(jobs.join(format!("{name}.tmp")), jobs.join(&name)).unwrap();
                if std::mem::take(&mut self.ending) {
                    Waiting::Gone(here.to_owned())
                } else {
                    Waiting::File(code.clone())
                }
            }
        };
        let status = self.serve(&mut waiting, run.turns);
        Said {
            status,
            out: std::fs::read_to_string(&out).unwrap_or_default(),
            err: std::fs::read_to_string(&err).unwrap_or_default(),
        }
    }

    /// Answers the Mac's conversations until the run is over, and returns its status.
    fn serve(&mut self, waiting: &mut Waiting, turns: &[&str]) -> i32 {
        let start = Instant::now();
        let mut turn = 0;
        loop {
            if let Some(app) = self.app.as_mut() {
                app.pump(Duration::from_millis(5));
                while app.tickets.len() > self.served {
                    let a = app.take_ticket(self.served, 65536);
                    app.until("the whole message", |b| b.flows[&a].eof);
                    let asked = String::from_utf8_lossy(&app.flows[&a].data).into_owned();
                    self.told.push(asked);
                    let reply = turns
                        .get(turn)
                        .map(|reply| (*reply).to_owned())
                        .or_else(|| self.reply.clone())
                        .unwrap_or_default();
                    app.stream_send(a, reply.as_bytes(), 4096);
                    app.stream_close(a);
                    self.served += 1;
                    turn += 1;
                }
            } else {
                std::thread::sleep(Duration::from_millis(5));
            }
            if let Some(status) = waiting.status(&self.env) {
                return status;
            }
            if start.elapsed() > LIMIT {
                let dir = self.env.dir.display().to_string();
                let ps: Vec<String> = self
                    .env
                    .processes()
                    .into_iter()
                    .filter(|(_, _, c)| c.contains(&dir))
                    .map(|(p, pp, c)| format!("{p} {pp} {c}"))
                    .collect();
                let files: Vec<String> = std::fs::read_dir(&dir)
                    .unwrap()
                    .filter_map(Result::ok)
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect();
                panic!(
                    "the command did not end: {:?} served {} told {:?}\nprocesses {ps:#?}\nfiles {files:?}",
                    waiting.describe(),
                    self.served,
                    self.told
                );
            }
        }
    }
}

enum Waiting {
    Child(Child),
    File(PathBuf),
    /// The terminal that runs the command is gone: the command ended it, and itself with it.
    Gone(String),
}

impl Waiting {
    fn describe(&self) -> String {
        match self {
            Waiting::Child(child) => format!("child {}", child.id()),
            Waiting::File(path) => format!("file {}", path.display()),
            Waiting::Gone(id) => format!("the end of {id}"),
        }
    }

    fn status(&mut self, env: &Env) -> Option<i32> {
        match self {
            Waiting::Child(child) => child
                .try_wait()
                .unwrap()
                .map(|status| status.code().unwrap_or(-1)),
            Waiting::File(path) => std::fs::read_to_string(path)
                .ok()
                .and_then(|text| text.trim().parse().ok()),
            Waiting::Gone(id) => {
                let listed = env.kablo().arg("list").output().unwrap();
                let runs = String::from_utf8_lossy(&listed.stdout).lines().any(|line| {
                    line.split_whitespace().next() == Some(id.as_str()) && !line.contains("ended")
                });
                (!runs).then_some(0)
            }
        }
    }
}

/// The first line of every message's frozen shape, for a test that writes one by hand.
pub fn parse_told(text: &str) -> kablo_bateri::message::Request {
    kablo_bateri::message::Request::parse(text).unwrap_or_else(|why| panic!("{text:?}: {why:?}"))
}

pub fn exists(path: &Path) -> bool {
    path.exists()
}

pub type Vars = HashMap<String, String>;
