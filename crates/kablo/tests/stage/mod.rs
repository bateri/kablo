//! The `bateri` command run for real: the real `kablo` binary, in a temporary home and socket
//! directory, against a test in the place of the bateri app (a connection to `kablo attach`, as the bateri app
//! is a program with one, the method of `app.rs`). What the command says to the bateri app is read back
//! and compared with `kablo-bateri`'s own reading, and what the bateri app says back is whatever the test
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

/// What the app puts in the shells it holds on its own machine: the address of the pane.
pub const PANE_HERE: &str = "BATERI_TAB_URL";

/// What `ssh` carries to a server: the address of the pane of the app that opened the connection.
pub const PANE_CARRIED: &str = "LC_BATERI_TAB_URL";

/// Whose app a run is in the pane of, which a run says by its environment.
#[derive(Clone, Copy, Debug)]
pub enum Pane {
    /// A pane of the app on this machine: both variables, as the app sets them.
    Local,
    /// A pane of the app on another machine, reached through `ssh`: the carried variable only.
    Carried,
    /// A shell of no app (cron, a plain `ssh`): neither variable.
    Bare,
}

/// One run: where it is, and what to do about the conversations it has.
pub struct Run<'a> {
    pub here: Option<&'a str>,
    pub args: &'a [&'a str],
    pub input: Option<&'a [u8]>,
    /// The bateri app's answer to the conversation of this turn (from 0), else the stage's.
    pub turns: &'a [&'a str],
    /// Variables of the command.
    pub vars: &'a [(&'a str, &'a str)],
}

pub struct Stage {
    pub env: Env,
    pub home: PathBuf,
    app: Option<Bridge>,
    served: usize,
    /// Every message the bateri app was told, in order.
    pub told: Vec<String>,
    reply: Option<String>,
    runs: usize,
    runners: Vec<String>,
    /// The next run ends the terminal it is run in: what is waited for is that.
    ending: bool,
    /// Variables every run has.
    vars: Vec<(String, String)>,
    /// The umask the runs in a terminal have.
    umask: Option<String>,
    /// The terminals the command started, which are ended with the stage.
    adopted: std::collections::HashSet<String>,
}

impl Stage {
    /// A stage with no bateri app connected.
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
            vars: Vec::new(),
            umask: None,
            adopted: std::collections::HashSet::new(),
        }
    }

    /// A stage whose bateri app says `reply` to whatever it is asked.
    pub fn with_app(reply: &str) -> Stage {
        let mut stage = Stage::new();
        stage.connect_app();
        stage.app_says(reply);
        stage
    }

    /// The bateri app connects.
    pub fn connect_app(&mut self) {
        if self.app.is_none() {
            let mut app = Bridge::start(&self.env);
            app.take_tickets();
            self.app = Some(app);
        }
    }

    /// The bateri app goes away.
    pub fn no_app(&mut self) {
        self.app = None;
        self.served = 0;
    }

    /// From now on the bateri app says `reply` to each conversation.
    pub fn app_says(&mut self, reply: &str) {
        self.connect_app();
        self.reply = Some(reply.to_owned());
    }

    /// The state the command keeps: `~/.local/share/bateri`.
    pub fn state(&self) -> PathBuf {
        self.home.join(".local/share/bateri")
    }

    /// Forgets what the bateri app was told so far.
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

    /// Every run has the variable `name`.
    pub fn set_var(&mut self, name: &str, value: &str) {
        self.vars.retain(|(have, _)| have != name);
        self.vars.push((name.to_owned(), value.to_owned()));
    }

    /// Every run is in `pane`.
    pub fn in_pane(&mut self, pane: Pane) {
        let url = "bateri://tab/5A1E2D3C-4B5A-6978-8796-A5B4C3D2E1F0";
        self.unset_var(PANE_HERE);
        self.unset_var(PANE_CARRIED);
        if matches!(pane, Pane::Local) {
            self.set_var(PANE_HERE, url);
        }
        if matches!(pane, Pane::Local | Pane::Carried) {
            self.set_var(PANE_CARRIED, url);
        }
    }

    /// No run has the variable `name`, whatever the test's own environment has.
    pub fn unset_var(&mut self, name: &str) {
        self.vars.retain(|(have, _)| have != name);
    }

    /// The runs in a terminal have this umask.
    pub fn set_umask(&mut self, mask: &str) {
        self.umask = Some(mask.to_owned());
    }

    /// The terminals the command started are this stage's to end.
    fn adopt(&mut self) {
        let Ok(entries) = std::fs::read_dir(&self.env.dir) else {
            return;
        };
        for entry in entries.filter_map(Result::ok) {
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(id) = name.strip_suffix(".sock") {
                if self.adopted.insert(id.to_owned()) && !self.runners.iter().any(|r| r == id) {
                    self.env.track_named_alike(id);
                }
            }
        }
    }

    /// The command as the bootstrap writes it: one line that hands what it is given to `kablo`.
    pub fn install_command(&mut self) {
        let bin = self.state().join("shell/bin");
        std::fs::create_dir_all(&bin).unwrap();
        let file = bin.join("bateri");
        std::fs::write(
            &file,
            format!(
                "#!/bin/sh\nexec '{}' bateri \"$@\"\n",
                env!("CARGO_BIN_EXE_kablo")
            ),
        )
        .unwrap();
        std::fs::set_permissions(&file, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
    }

    /// The bootstrap of a server's terminal: a program that says what it was started with and
    /// holds the terminal open.
    pub fn install_bootstrap(&mut self) {
        let dir = self.state().join("shell");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("boot.sh"),
            format!(
                "printf '%s|' \"$@\" > '{0}/boot.args'\nprintf '%s' \"$SHELL\" > '{0}/boot.shell'\nexec cat\n",
                self.home.display()
            ),
        )
        .unwrap();
    }

    /// What the bootstrap was started with: its arguments joined by `|`, and the `SHELL` it had.
    pub fn bootstrap_saw(&self) -> Option<(String, String)> {
        Some((
            std::fs::read_to_string(self.home.join("boot.args")).ok()?,
            std::fs::read_to_string(self.home.join("boot.shell")).ok()?,
        ))
    }

    /// The message that waits for the bateri app under `id`.
    pub fn note(&self, id: &str) -> Option<String> {
        std::fs::read_to_string(self.state().join("placing").join(id)).ok()
    }

    /// Who opened `id`, as the command wrote it.
    pub fn record(&self, id: &str) -> Option<String> {
        std::fs::read_to_string(self.state().join("terminals").join(id)).ok()
    }

    /// The terminals of this machine that run, by id.
    pub fn running(&self) -> Vec<String> {
        let listed = self.env.kablo().arg("list").output().unwrap();
        String::from_utf8_lossy(&listed.stdout)
            .lines()
            .skip(1)
            .filter(|line| !line.contains("ended"))
            .filter_map(|line| line.split_whitespace().next().map(str::to_owned))
            .collect()
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
            "#!/bin/sh\ntrap '' INT\njobs=$1\nwhile :; do\n  for f in \"$jobs\"/job.*.sh; do\n    [ -f \"$f\" ] || continue\n    run=\"$jobs/running.${f##*/}\"\n    mv \"$f\" \"$run\"\n    sh \"$run\"\n    rm -f \"$run\"\n  done\n  sleep 0.02\ndone\n",
        );
        let kablo = env!("CARGO_BIN_EXE_kablo");
        let started = self
            .env
            .kablo()
            .args(["new", "--id", here])
            .arg("--cwd")
            .arg(&self.home)
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

    /// A run with its own turns of the bateri app's.
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
                    .current_dir(&self.home)
                    .env("PWD", &self.home)
                    .arg("bateri")
                    .args(run.args)
                    .env("KABLO_DIR", &self.env.dir)
                    .env("HOME", &self.home)
                    .env_remove(PANE_HERE)
                    .env_remove(PANE_CARRIED)
                    .envs(self.vars.iter().map(|(n, v)| (n, v)))
                    .envs(run.vars.iter().copied())
                    .stdin(Stdio::from(File::open(&input).unwrap()))
                    .stdout(Stdio::from(File::create(&out).unwrap()))
                    .stderr(Stdio::from(File::create(&err).unwrap()));
                Waiting::Child(command.spawn().unwrap())
            }
            Some(here) => {
                let jobs = self.runner(here);
                let mut job = String::from("\"$KABLO\" bateri");
                let mask = self.umask.clone();
                for arg in run.args {
                    job.push_str(&format!(" '{}'", arg.replace('\'', "'\\''")));
                }
                for (name, value) in run.vars {
                    job = format!("{name}='{value}' {job}");
                }
                for (name, value) in &self.vars {
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
                // The test says whose pane the run is in; the pane of whoever runs the tests is
                // not it.
                job = format!("unset {PANE_HERE} {PANE_CARRIED}\n{job}");
                if let Some(mask) = mask {
                    job = format!("umask {mask}\n{job}");
                }
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
        self.adopt();
        Said {
            status,
            out: std::fs::read_to_string(&out).unwrap_or_default(),
            err: std::fs::read_to_string(&err).unwrap_or_default(),
        }
    }

    /// Answers the bateri app's conversations until the run is over, and returns its status.
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
