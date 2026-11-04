//! Another machine: how to reach it, and the command run there.
//!
//! A pane written `p9@prod` is on a machine other than the one that runs this. The Mac is the one
//! that reaches machines (it holds the connections, and asks the person for a password when one has
//! to be made), so it is asked first, and says how: the words of an `ssh` command that rides the
//! connection it keeps; `relay` when the asker is itself a program on a server, which has no `ssh`
//! to the other machine, and the Mac runs the command there on its behalf; or `here`, when the
//! machine named is the asker's own and there is nothing to reach.

use std::io::Read;
use std::process::{Command, Stdio};

use kablo_bateri::message::{Reach, Relay, Reply, Request};

use super::{Asked, Ctx, Done, Exit, Io, failure, finish, unread};

/// The alphabet a quoted word is built from, as every shell a remote command line may meet reads it.
fn quote(word: &str) -> String {
    let mut quoted = String::from("'");
    for c in word.chars() {
        if c == '\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(c);
        }
    }
    quoted.push('\'');
    quoted
}

/// The words as the other machine's `bateri --hex` takes them, a space before each: `x` and the hex
/// of its bytes. Nothing the caller wrote is ever read by a shell on the way, whichever shell that
/// is.
pub fn hex_words(words: &[String]) -> String {
    let mut text = String::new();
    for word in words {
        text.push_str(" x");
        for byte in word.as_bytes() {
            text.push_str(&format!("{byte:02x}"));
        }
    }
    text
}

/// What `bateri --hex` reads: words written as `x` and the hex of their bytes, back as the words.
pub fn unhex_words(words: &[String]) -> Result<Vec<String>, Exit> {
    let wrong = || Exit::mistake("--hex takes words written as x and their bytes in hex");
    let mut out = Vec::new();
    for word in words {
        let digits = word.strip_prefix('x').ok_or_else(wrong)?;
        if digits.len() % 2 != 0 || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(wrong());
        }
        let mut bytes = Vec::with_capacity(digits.len() / 2);
        for pair in digits.as_bytes().chunks(2) {
            let pair = std::str::from_utf8(pair).map_err(|_| wrong())?;
            let byte = u8::from_str_radix(pair, 16).map_err(|_| wrong())?;
            if byte == 0 {
                return Err(Exit::mistake("a word has a NUL in it"));
            }
            bytes.push(byte);
        }
        out.push(String::from_utf8_lossy(&bytes).into_owned());
    }
    Ok(out)
}

/// The line the other machine's shells read, however the person's login shell reads a line: fixed
/// text, and the words in hex (`bateri --hex`). A `sh` hands over to the login shell, which runs a
/// `sh` that looks for the command where the files of that machine are, and says so when it is
/// nowhere (nothing is installed there: a server bateri never opened is told to be).
pub fn remote_line(server: &str, words: &[String]) -> String {
    let search = format!(
        "for d in \"${{XDG_DATA_HOME:-$HOME/.local/share}}\" \"$HOME/.local/share\"; do [ -x \"$d/bateri/shell/bin/bateri\" ] && exec env BATERI_QUIET_ORPHAN=1 \"$d/bateri/shell/bin/bateri\" --hex \"$@\"; done; echo \"bateri: bateri is not set up on {server}: open a terminal on it in bateri first\" >&2; exit 127"
    );
    let login = format!("exec sh -c {} sh{}", quote(&search), hex_words(words));
    let line = format!(
        "exec sh -c {} sh {}",
        quote("exec \"${SHELL:-/bin/sh}\" -lc \"$1\""),
        quote(&login)
    );
    line
}

/// How the Mac says `server` is reached.
pub fn reach(ctx: &Ctx, server: &str) -> Result<Reach, Exit> {
    match ctx.ask(&Request::Ssh(server.to_owned()))? {
        Asked::NoMac => Err(Exit::new(
            69,
            format!("no Mac is connected: it is the Mac that reaches {server}"),
        )),
        Asked::Silent => Err(Exit::failed("the Mac did not answer")),
        Asked::Unreadable => Err(unread()),
        Asked::Reply(Reply::Text(text)) => Reach::parse(&text).map_err(|_| unread()),
        Asked::Reply(Reply::Failed(failed)) => Err(failure(&failed)),
        Asked::Reply(_) => Err(unread()),
    }
}

/// What becomes of what a command run on another machine prints.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Output {
    /// It is printed here, as it would have been.
    Print,
    /// It is kept and given back.
    Capture,
    /// It is thrown away, what it says on standard error too.
    Quiet,
}

/// What a command run on another machine said, when it was asked to say it here.
pub struct Ran {
    /// How it ended: its status, or 255 for ssh that could not connect and for kablo's own trouble.
    pub status: u8,
    /// What it printed, when it was not let print.
    pub stdout: String,
}

/// `bateri WORDS` of `server`, run there the way the Mac said (`reach`): over ssh, or by the Mac
/// when this is a program on a server. Its standard input is this one's, or none.
pub fn remote(
    ctx: &Ctx,
    io: &mut Io<'_>,
    way: &Reach,
    server: &str,
    words: &[String],
    output: Output,
    input: bool,
) -> Result<Ran, Exit> {
    match way {
        Reach::Relay => relay(ctx, io, server, words, output),
        Reach::Words(ssh) => {
            let _ = io.out.flush();
            let _ = io.err.flush();
            let mut command = Command::new("ssh");
            command.args(ssh).arg(remote_line(server, words));
            command.stdin(if input {
                Stdio::inherit()
            } else {
                Stdio::null()
            });
            command.stderr(if output == Output::Quiet {
                Stdio::null()
            } else {
                Stdio::inherit()
            });
            command.stdout(match output {
                Output::Print => Stdio::inherit(),
                Output::Capture => Stdio::piped(),
                Output::Quiet => Stdio::null(),
            });
            let child = command.spawn();
            let mut child = match child {
                Ok(child) => child,
                Err(error) => {
                    io.say(&format!("cannot run ssh: {error}"));
                    return Ok(Ran {
                        status: 255,
                        stdout: String::new(),
                    });
                }
            };
            let mut stdout = Vec::new();
            if let Some(pipe) = child.stdout.as_mut() {
                let _ = pipe.read_to_end(&mut stdout);
            }
            let status = match child.wait() {
                Ok(status) => status_of(&status),
                Err(_) => 255,
            };
            Ok(Ran {
                status,
                stdout: String::from_utf8_lossy(&stdout).into_owned(),
            })
        }
        // The machine is this one: the caller does not call this.
        Reach::Here => Err(Exit::failed(
            "a machine that is this one is reached by nothing",
        )),
    }
}

/// A process's end as a status: its own, or 128 and the signal that ended it.
fn status_of(status: &std::process::ExitStatus) -> u8 {
    use std::os::unix::process::ExitStatusExt;
    match (status.code(), status.signal()) {
        (Some(code), _) => u8::try_from(code & 0xff).unwrap_or(255),
        (None, Some(signal)) => u8::try_from(128 + signal).unwrap_or(255),
        (None, None) => 255,
    }
}

/// `bateri WORDS` on `server`, run by the Mac. The words go in the message (a line break in a `send`
/// cannot ride a line on its own), and what comes back is the command's status, a few lines of its
/// stderr and its stdout as it was, which are printed as they would have been. `send ID -` reads its
/// text here, since standard input does not cross.
fn relay(
    ctx: &Ctx,
    io: &mut Io<'_>,
    server: &str,
    words: &[String],
    output: Output,
) -> Result<Ran, Exit> {
    let mut words = words.to_vec();
    if words.first().is_some_and(|verb| verb == "send") {
        let mut text = Vec::new();
        for word in words.iter_mut().skip(1) {
            if word == "-" {
                if text.is_empty() {
                    let _ = std::io::stdin().lock().read_to_end(&mut text);
                }
                *word = String::from_utf8_lossy(&text).into_owned();
            }
        }
    }
    let request = Request::Relay(Relay {
        server: server.to_owned(),
        words,
    });
    match ctx.ask(&request)? {
        Asked::NoMac => Err(Exit::new(
            69,
            format!("no Mac is connected: it is the Mac that runs this on {server}"),
        )),
        Asked::Silent => Err(Exit::failed("the Mac did not answer")),
        Asked::Unreadable => Err(unread()),
        Asked::Reply(Reply::Relayed(relayed)) => {
            if output != Output::Quiet {
                for line in &relayed.stderr {
                    let _ = writeln!(io.err, "{line}");
                }
            }
            match output {
                Output::Capture => Ok(Ran {
                    status: relayed.status,
                    stdout: relayed.stdout,
                }),
                Output::Print => {
                    let _ = io.out.write_all(relayed.stdout.as_bytes());
                    Ok(Ran {
                        status: relayed.status,
                        stdout: String::new(),
                    })
                }
                Output::Quiet => Ok(Ran {
                    status: relayed.status,
                    stdout: String::new(),
                }),
            }
        }
        Asked::Reply(Reply::Failed(failed)) => Err(failure(&failed)),
        Asked::Reply(_) => Err(unread()),
    }
}

/// `bateri WORDS` for a pane of the machine `via`, run where it is. This Mac's own panes (`@local`)
/// are the plain pane, and the Mac says so when it is asked to reach them (`here`).
pub fn carry(ctx: &Ctx, io: &mut Io<'_>, via: &str, words: &[String]) -> Done {
    let via = if via == "local" { "@local" } else { via };
    match reach(ctx, via)? {
        Reach::Here => super::run_words(ctx, io, words),
        way => {
            let ran = remote(ctx, io, &way, via, words, Output::Print, true)?;
            if ran.status == 255 {
                io.say(&format!(
                    "could not reach {via} over ssh, or kablo had trouble there"
                ));
            }
            finish(ran.status)
        }
    }
}
