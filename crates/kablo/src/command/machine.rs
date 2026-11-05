//! Another machine: how to reach it, and the command run there.
//!
//! A pane written `p9@prod` is on a machine other than the one that runs this. The bateri app is
//! the one that reaches machines (it holds the connections, and asks the person for a password
//! when one has to be made), so it is asked first, and says how: `relay`, and the app runs the
//! command there on the asker's behalf; or `here`, when the machine named is the asker's own and
//! there is nothing to reach.

use std::io::Read;

use kablo_bateri::message::{Reach, Relay, Reply, Request, word_of_hex};

use super::{Asked, Ctx, Done, Exit, Io, failure, finish, unread};

/// What `bateri --hex` reads: words written as `x` and the hex of their bytes, back as the words.
pub fn unhex_words(words: &[String]) -> Result<Vec<String>, Exit> {
    let mut out = Vec::new();
    for word in words {
        let word = word_of_hex(word)
            .map_err(|_| Exit::mistake("--hex takes words written as x and their bytes in hex"))?;
        if word.contains('\0') {
            return Err(Exit::mistake("a word has a NUL in it"));
        }
        out.push(word);
    }
    Ok(out)
}

/// How the bateri app says `server` is reached.
pub fn reach(ctx: &Ctx, server: &str) -> Result<Reach, Exit> {
    match ctx.ask(&Request::Ssh(server.to_owned()))? {
        Asked::NoApp => Err(Exit::new(
            69,
            format!("the bateri app is not connected: it is what reaches {server}"),
        )),
        Asked::Silent => Err(Exit::failed("the bateri app did not answer")),
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
    /// How it ended: its status, or 255 when the server could not be reached and for kablo's own
    /// trouble.
    pub status: u8,
    /// What it printed, when it was not let print.
    pub stdout: String,
}

/// `bateri WORDS` on `server`, run by the bateri app. The words go in the message (a line break in
/// a `send` cannot ride a line on its own), and what comes back is the command's status, a few
/// lines of its stderr and its stdout as it was, which are printed as they would have been. `send
/// ID -` reads its text here, since standard input does not cross.
pub fn remote(
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
        Asked::NoApp => Err(Exit::new(
            69,
            format!("the bateri app is not connected: it is what runs this on {server}"),
        )),
        Asked::Silent => Err(Exit::failed("the bateri app did not answer")),
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

/// `bateri WORDS` for a pane of the machine `via`, run where it is. This computer's own panes
/// (`@local`) are the plain pane, and the bateri app says so when it is asked to reach them
/// (`here`).
pub fn carry(ctx: &Ctx, io: &mut Io<'_>, via: &str, words: &[String]) -> Done {
    let via = if via == "local" { "@local" } else { via };
    match reach(ctx, via)? {
        Reach::Here => super::run_words(ctx, io, words),
        Reach::Relay => {
            let ran = remote(ctx, io, via, words, Output::Print)?;
            if ran.status == 255 {
                io.say(&format!(
                    "could not reach {via}, or kablo had trouble there"
                ));
            }
            finish(ran.status)
        }
    }
}
