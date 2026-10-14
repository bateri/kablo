# kablo

A transparent terminal session keeper. kablo holds the terminal (PTY) of a
shell so the programs in it outlive the connection that started them: close
the laptop, lose the network, restart the terminal app — the shell and
everything running in it keep going, and the next connection picks up where
the last one left off.

Unlike tmux or screen, kablo draws nothing. It does not put a terminal of its
own between the program and you; while a client is attached, the bytes the
program writes reach the client unchanged. Windows, tabs and splits belong to
the terminal app, not to kablo.

kablo is built to be driven by a terminal app — locally over a unix socket,
remotely over a plain `ssh host kablo attach` — and needs nothing from the
server beyond a login and the ability to run one command. It also works on
its own, the way `dtach` does (below).

Status: early development, not yet released. Linux (x86_64, aarch64) and macOS
(Apple silicon, Intel).

## Install

There is no release yet. Build it from source, with Rust 1.88 or newer:

```sh
cargo build --release          # target/release/kablo
```

`make dist` builds what a release will ship: `dist/kablo-<version>-<target>`
for the four targets, and a `SHA256SUMS` next to them. It needs a Mac with
Docker (the Linux binaries are static musl builds, made and tried in a
container of their own architecture), a local `rustc` of the version named in
`tools/linux/Dockerfile` (as for `make linux`), and rustup with both Apple
targets installed in its `stable` toolchain (`rustup target add
aarch64-apple-darwin x86_64-apple-darwin --toolchain stable`; Homebrew's Rust
cannot add them). The macOS binaries are built by that toolchain, the Linux
ones by the compiler in the image. Check what you downloaded with
`shasum -a 256 --ignore-missing -c SHA256SUMS` (`sha256sum --ignore-missing -c`
on Linux), which skips the binaries you did not download.

The binary is one file. Put it where your shell finds it, on a server too. The
program a terminal app runs over ssh is `kablo attach`, and ssh runs it in a
non-interactive shell, whose `PATH` often leaves out `~/.local/bin`; apps
therefore give ssh the path itself
(`ssh host .local/share/kablo/0.1.0/kablo attach`, see *For apps that drive
kablo*).

## Use

```sh
kablo new                  # start a session; prints its id
kablo new --id build -- make -j8
kablo list                 # the sessions on this machine
kablo connect build        # use one from this terminal
```

`kablo new` starts your login shell, or the program you give after `--`, in a
session that goes on without you. `--id NAME` names it (letters, digits, `.`,
`_` and `-`; up to 64), `--cols` and `--rows` give it a size, `--cwd DIR` a
directory, `--env NAME=VALUE` an addition to the environment, and
`--journal-size 32M` how much output is kept for a client that comes back
(16K to 1G; the default is 8M).

`kablo connect ID` puts the session on your terminal: what you type goes to
the program, what it writes goes to your screen, and resizing the window
resizes the program's terminal. **Ctrl-\ leaves the session running** and gives
the terminal back; ending the program (`exit`) ends the session, and
`connect` exits with the program's own status. Connecting again shows what the
session printed while you were gone: kablo keeps the bytes the program wrote,
so a full-screen program is drawn again from the start of what is kept, and
the last frame wins. `--fresh` skips the past and shows only what comes next.
When the session cannot take what you type for a moment, `connect` holds it
and tries again (a few seconds at most, then it says why and exits); when the
program is not reading its input at all, the terminal rings its bell for the
keys that were refused and `connect` stays, so Ctrl-\ still works.

`kablo list` is a table for people and `kablo list --json` is JSON for
programs (keys are added to it, never removed). A session whose shell lives
but whose relay keeps failing is listed as `broken`.

Over ssh the same commands work from a terminal: `ssh -t host kablo connect
build`.

Sessions are kept in `/tmp/kablo-<uid>/`; `KABLO_DIR` points kablo somewhere
else, which is what the tests do.

## Staying alive after you log out

kablo starts each session detached from your login (`setsid`), so closing the
ssh connection does not take it along. Some Linux systems go further and end
every process of a user who has logged out: systemd with
`KillUserProcesses=yes`. There, a session survives only if your user lingers:

```sh
loginctl enable-linger "$USER"
```

kablo notices that setting (the file `/var/lib/systemd/linger/<user>`) and
starts the session in a systemd scope of its own (`systemd-run --user
--scope`). Without it kablo does not use systemd at all: the user manager of
someone who does not linger stops soon after the last login and would take
the scope with it. Most distributions ship `KillUserProcesses=no` and need
nothing, and macOS needs nothing.

## Over ssh

```sh
ssh host kablo attach
```

`kablo attach` is not for people: it speaks the kablo protocol on its standard
input and output, and that one connection carries **every** session on the
server — listing them, starting new ones, attaching to any number — so sshd's
limit on sessions per connection never comes into it. When the ssh connection
breaks the sessions go on, and a client that connects again asks for the bytes
after the last one it received and gets exactly those. If a session's relay
dies or is replaced while a client is attached, `attach` connects again and
the stream continues without a gap, a repeat, or a word to the client.

What a terminal app does with that is its own business; the protocol is in
[docs/PROTOCOL.md](docs/PROTOCOL.md).

## Replacing kablo under running sessions

Each session is two processes: a *warden*, which is the shell's parent and
never changes while the shell lives, and a *relay*, which listens for clients,
reads the terminal and keeps the journal. The relay can be swapped for the one
in another binary without ending the shell and without dropping a client:

```sh
kablo upgrade ID /path/to/new/kablo
kablo upgrade --all /path/to/new/kablo
```

The path must be absolute (a relative one is made absolute from where you run
the command), a regular file you can run, owned by you or by root, and
writable by nobody else. On success `upgrade` prints `ID: relay replaced (now
VERSION)` for each session and exits 0; a client attached to the session is
reconnected by its `attach` without noticing. On failure it says why and
exits 1 (the file is not one to run, the program has ended, the new relay
stopped before it took over), and the old relay keeps serving. The warden is
never replaced: a new warden only comes with a new session.

Put each version in a directory of its own and leave the old one where it is.
Overwriting the binary of a running session in place costs the warden its
memory of where its relay comes from (see *Limits*).

## Security

- Sessions live in a directory only you can use: `/tmp/kablo-<uid>/`, mode
  0700, owned by you. Anything else is refused, not repaired. Every
  connection to a session's socket is checked for the uid of the process on
  the other end (`SO_PEERCRED`, `getpeereid`), and another user's is closed.
- kablo opens no network port. Remote access is your ssh login running one
  command; whoever can log in as you can use your sessions, as they can use
  any other process of yours.
- A new session's program is an argument vector, never a shell string: nothing
  that arrives over the wire is joined into a command line, and the arguments
  and environment are not written to any log.
- The journal holds everything the program wrote, secrets it printed
  included. It is kept in memory (a `memfd` on Linux, anonymous shared memory
  on macOS), not in a file, up to `--journal-size` per session, and it goes
  when the session does.

## Limits

- **No screen snapshot yet.** kablo keeps bytes, not a picture of the screen.
  A client that comes back gets the bytes it missed. If the journal has
  wrapped, it gets the rest from the start of the first line that is left and
  is told the earlier part is gone, and a full-screen program is nudged to
  redraw (the terminal is made one row smaller and back). A program that
  changes the screen without drawing it all again can look wrong until it
  redraws.
- **A program that does not read its input** gets only so much of it. The
  session holds 1 MiB of typed bytes for it; past that, an `input` is not
  written and its sender is told (`INPUT_FULL` in the protocol; through the
  bridge it reaches the client unchanged). Everything else about the session
  keeps working: `status`, `list`, `resize`, `detach` and `terminate` are
  answered, and its output goes on flowing. The refused bytes are not queued
  anywhere, so send them again once the program reads.
- **There is no command to end a session by force.** Apps do it with the
  protocol's `terminate`, which hangs up the shell; a program that ignores the
  hang-up, a stuck one, or a `broken` session is ended with `kill`: find the
  keeper in `ps` (`kablo warden --id ID`) and stop it and the program under
  it.
- **Input typed at the instant a relay is replaced** (`kablo upgrade`) can be
  lost; output is not.
- **The journal is bounded** (8 MiB by default). Output the program writes
  while nobody is attached is kept only as far as that.
- **An ended session** is kept until a client has been told how it ended, a
  day at most, and then removed.
- **A binary overwritten in place.** On Linux, a running warden that was
  started from a file which has since been replaced reads its own path as
  `… (deleted)`. If its relay dies before you run `kablo upgrade` for that
  session, the warden cannot start another one and the session stays listed
  as `broken` (the shell lives on). Install new versions beside the old ones
  and `kablo upgrade` to them.
- **`upgrade` can fall back without saying so.** If the new relay dies between
  announcing itself and taking over, or the old one dies during the hand-over,
  the warden starts a relay from the binary it had before, and `kablo upgrade`
  still prints `relay replaced`. The version in that line is the only hint:
  compare it with the one you installed.
- **The systemd scope** is tested against a stand-in `systemd-run`, not yet on
  a machine with a real systemd. The `setsid` route is what the end-to-end
  tests (a real sshd in a container) run.
- Windows and the BSDs are not supported, and a server with SELinux enforcing
  has not been tried (the ssh route asks for no socket or port forwarding).

## For apps that drive kablo

- **The protocol** is in [docs/PROTOCOL.md](docs/PROTOCOL.md) and implemented
  by the `kablo-proto` crate, which does no I/O and has no dependencies. A
  build reads its own wire version and the one before it, and the bytes of
  every message of every shipped version are kept as test fixtures.
  `kablo version` prints one line of `key=value` tokens
  (`version=… wire_min=… wire_max=…`): keys may be added to it, none removed.
- **Putting kablo on a server** follows one set of rules, in
  `kablo_proto::install`, so that two apps do not leave two copies behind:

  ```rust
  use kablo_proto::install::{self, Target};

  // `ssh host uname -s -m` (install::UNAME_COMMAND) printed `output`
  let target = Target::from_uname_output(&output).ok_or("no kablo for this server")?;
  install::artifact_name("0.1.0", target)?;        // kablo-0.1.0-x86_64-unknown-linux-musl
  install::install_path("0.1.0")?;                 // .local/share/kablo/0.1.0/kablo
  install::install_path_in("/home/ada", "0.1.0")?; // the same, for `kablo upgrade`
  ```

  The binary of a release is listed with its SHA-256 in `SHA256SUMS`; check
  it before it goes to the server. A version has a directory of its own below
  the home directory and a new version never overwrites an old one, so a relay
  that runs from the old file is not disturbed.

## License

kablo is free software: you can redistribute it and/or modify it under the
terms of the [GNU General Public License](LICENSE) as published by the Free
Software Foundation, either version 3 of the License, or (at your option) any
later version. It is distributed in the hope that it will be useful, but
WITHOUT ANY WARRANTY; see the license for details.

Copyright © 2026 Ömer Kala.
