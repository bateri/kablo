//! Which session a process comes from, with real sessions: the shell is the
//! one `kablo new` starts, and what the origin search is given is what the
//! sessions answer to `status`.

mod common;

use common::Env;
use kablo::origin::{Candidate, Origin, find};
use kablo::sys;
use kablo_proto::SessionId;

const SH: [&str; 2] = ["--", "/bin/sh"];

/// The pid in `child-<pid>-end`, once the shell has printed it. The shell's
/// echo of the command line says `child-$$-end`, which is not a number.
fn printed(data: &[u8]) -> Option<u32> {
    let text = String::from_utf8_lossy(data);
    text.split("child-")
        .skip(1)
        .find_map(|rest| rest.split_once("-end")?.0.parse().ok())
}

#[test]
fn a_command_of_a_session_comes_from_that_session_and_a_stranger_from_none() {
    let mut env = Env::new();
    let first = env.start("origin-a", &SH);
    let second = env.start("origin-b", &SH);

    let mut shell = env.connect(&first);
    shell.attach(0);
    shell.type_in("sh -c 'echo child-$$-end; exec sleep 60' &\n");
    shell.until("the shell to print the pid", |c| printed(&c.data).is_some());
    let program = printed(&shell.data).unwrap();

    // What a caller has: every session's `status`, and from it the running
    // ones.
    let candidates: Vec<Candidate> = [&first, &second]
        .into_iter()
        .filter_map(|id| Candidate::of(&env.connect(id).status()))
        .collect();
    assert_eq!(candidates.len(), 2);
    let found = find(program, &candidates);
    let stranger = find(std::process::id(), &candidates);
    sys::kill(program as libc::pid_t, libc::SIGKILL).unwrap();

    assert_eq!(found, Origin::Terminal(SessionId::new(&first).unwrap()));
    assert_eq!(stranger, Origin::None);
}
