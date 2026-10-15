# kablo

A transparent terminal session keeper. kablo holds the terminal (PTY) of a
shell so the programs in it outlive the connection that started them: close
the laptop, lose the network, restart the terminal app — the shell and
everything running in it keep going, and the next connection picks up where
the last one left off.

Unlike tmux or screen, kablo draws nothing. It does not put a terminal of its
own between the program and you; while a client is attached, the bytes the
program writes reach the client unchanged. Windows, tabs and splits belong to
the terminal app, not to kablo. It does keep a screen of its own on the side,
fed by the same bytes: a client that has been away longer than the journal
reaches gets a picture of that screen first, then the program's output as it
comes, and programs that ask their terminal something while nobody is attached
are answered.

kablo is built to be driven by a terminal app — locally over a unix socket,
remotely over a plain `ssh host kablo attach` — and needs nothing from the
server beyond a login and the ability to run one command. It also works on
its own, the way `dtach` does (below).

Status: early; [0.1.0](CHANGELOG.md) is the first release. Linux (x86_64,
aarch64) and macOS (Apple silicon, Intel).

## Install

Download the binary for your machine from the
[latest release](https://github.com/bateri/kablo/releases/latest). Its name
ends in the target: `x86_64-unknown-linux-musl` or `aarch64-unknown-linux-musl`
(static, for any Linux distribution), `aarch64-apple-darwin` (Apple silicon)
or `x86_64-apple-darwin` (Intel).

```sh
v=0.1.0                        # the version of the release you download
t=x86_64-unknown-linux-musl    # your machine's target, from the list above
curl -fLO https://github.com/bateri/kablo/releases/download/v$v/kablo-$v-$t
curl -fLO https://github.com/bateri/kablo/releases/download/v$v/SHA256SUMS
grep -F " kablo-$v-$t" SHA256SUMS | sha256sum -c -    # on a Mac: ... | shasum -a 256 -c -
chmod +x kablo-$v-$t
mkdir -p ~/.local/bin && mv kablo-$v-$t ~/.local/bin/kablo
```

`SHA256SUMS` lists every file of the release, and the `grep` picks the line of
the one you downloaded. The release also carries this project's `LICENSE`
and `THIRD-PARTY-LICENSES.txt`, the notice for the code inside the binary that
asks to travel with copies of it.

**The macOS binaries are not signed or notarized.** A file `curl` fetched runs
as it is. One a browser saved carries macOS's quarantine mark, and Gatekeeper
refuses to run it until you remove the mark:

```sh
xattr -d com.apple.quarantine kablo-0.1.0-aarch64-apple-darwin
```

To build it from source instead, with Rust 1.88 or newer:

```sh
cargo build --release          # target/release/kablo
```

`make dist` builds the files a release ships: `dist/kablo-<version>-<target>`
for the four targets, with `LICENSE`, `THIRD-PARTY-LICENSES.txt` and the
`SHA256SUMS` over all of them. It needs a Mac with Docker (the Linux binaries
are static musl builds, made and tried in a container of their own
architecture), a local `rustc` of the version named in
`tools/linux/Dockerfile` (as for `make linux`), and rustup with both Apple
targets installed in its `stable` toolchain (`rustup target add
aarch64-apple-darwin x86_64-apple-darwin --toolchain stable`; Homebrew's Rust
cannot add them). The macOS binaries are built by that toolchain, the Linux
ones by the compiler in the image. `make release` runs every gate and then
`make dist` into `target/release-<version>/`, and `make ship` publishes the
result as a GitHub release; a version is released only if `CHANGELOG.md` has
a section for it.

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
(16K to 1G; the default is 8M). `--history 5000` is how many rows of scrollback
the session's screen keeps for the picture (0 to 100000; the default is 2000).

`kablo connect ID` puts the session on your terminal: what you type goes to
the program, what it writes goes to your screen, and resizing the window
resizes the program's terminal. **Ctrl-\ leaves the session running** and gives
the terminal back; ending the program (`exit`) ends the session, and
`connect` exits with the program's own status. Connecting again shows what the
session printed while you were gone: if the journal still reaches back to the
start, you get the bytes the program wrote; if not, your terminal is reset and
gets a picture of the session's screen (the screen, the scrollback it kept, the
modes the program set), and the program's own output goes on from there.
`--fresh` skips the past and shows only what comes next.
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

## Forwarding a port

```sh
kablo forward 8080:localhost:3000 -- ssh devbox kablo attach
```

`kablo forward` does what `ssh -L 8080:localhost:3000 devbox` does: connections
to port 8080 on this machine are carried to port 3000 on the server. The
difference is the road they take. They go through the one connection of
`kablo attach`, so sshd is asked for no forwarding at all, and it works where
sshd has forwarding turned off (`AllowTcpForwarding no`, `DisableForwarding
yes`). The command after `--` is run as it is, so ssh's options, jump hosts, keys
and password prompts stay in your ssh configuration: anything that ends in
`kablo attach` on the server will do.

```sh
kablo forward [--bind ADDRESS] (PORT | LOCAL:HOST:PORT)... -- COMMAND [ARGUMENT]...
```

- `PORT` is the same port here and on the server's `localhost`;
  `LOCAL:HOST:PORT` names the local port and where the server connects to. The
  **server** resolves and connects to `HOST` (a name or an address, IPv6 in
  brackets, `localhost` being the server's own), as `ssh -L` does. Give as many
  forwardings as you like.
- A local port of 0 lets the system choose. Once the server has said it takes
  connections, `kablo forward` prints a line on standard output for each
  address it listens on, `forwarding 127.0.0.1:41234 to localhost:3000`; a
  script can wait for it and connect.
- **Only the loopback listens** (`127.0.0.1` and, where there is one, `::1`)
  unless you say otherwise with `--bind ADDRESS` (an IP address): a forwarded
  port is a way into the server's network for whoever can connect to it. (As
  with `ssh -L`, that includes the other users of the machine `kablo forward`
  runs on, since a loopback port is open to all of them.)
- A port that is taken, a server whose kablo is too old to carry connections
  and a server whose administrator turned this off (below) are each said at
  once, and nothing keeps running. A target that cannot be reached is said and
  that connection closes; the others go on.
- When the command ends, `kablo forward` closes the connections and exits with
  status 1; it does not connect again. Ctrl-C (or a `SIGTERM`) ends it with
  128 plus the signal's number.
- The connections share the pipe with the sessions that travel over the same
  `kablo attach`, and neither waits behind the other: what you type and what
  a program prints are not held up by a large download, and a program that
  never stops printing does not stop a transfer. (The buffers of ssh and TCP
  themselves are still in the road.)

**What it can reach.** The server connects wherever its user can: the bridge is
the user's own process, and anything it can reach it could reach with `nc` or
`socat`. That is the same as `ssh -L`, and it is what sshd's own manual says
about `AllowTcpForwarding no`: it does not keep a user who has a shell from
forwarding. kablo opens no port on the server, only connections out of it.

**Turning it off on a server.** An administrator who does not want connections
carried through kablo creates the file `/etc/kablo/no-forwarding` (its contents
are not read). Every bridge, running ones included, then refuses new
connections: `kablo forward` says "turned off by the server's administrator"
and apps are told the same in the protocol. This states an intent so that kablo
does not quietly go around a decision; it is not a barrier, because a user can
run a forwarder of their own, as with any program that can open a connection.
Connections that are already open are not closed. If the bridge cannot tell
whether the file is there (its directory is not one the user may look in), it
counts as if it were, and says that it could not check.

## Replacing kablo under running sessions

Each session is two processes: a *warden*, which is the shell's parent, and a
*relay*, which listens for clients, reads the terminal and keeps the journal.
An upgrade replaces both, without ending the shell and without dropping a
client:

```sh
kablo upgrade ID /path/to/new/kablo
kablo upgrade --all /path/to/new/kablo
```

The relay is swapped for the one in the new binary first. Then the warden
replaces its own program with the same binary, in the same process: its pid
stays, the shell stays its child (so its exit code is still collected), and the
terminal, the journal and the listening socket stay open. It does that at a
quiet moment (the shell lives and nothing is being handed over), which comes
right after the relay took over unless something is in the middle of a change.

The path must be absolute (a relative one is made absolute from where you run
the command), a regular file you can run, owned by you or by root, and
writable by nobody else. On success `upgrade` prints `ID: replaced (now
VERSION)` for each session and exits 0; a client attached to the session is
reconnected by its `attach` without noticing. On failure it says why and
exits 1 (the file is not one to run, the program has ended, the new relay
stopped before it took over), and the old relay keeps serving.

The warden stays as it was when it cannot follow: the new relay does not say it
reads what a renewing warden leaves for it, the program ended before the
quiet moment came, the new binary could not be run again from where it is, or
the warden is from a release before wardens could be renewed. `upgrade` then
still exits 0, because the relay was replaced, and says so: `ID: relay
replaced (now VERSION); its warden stays at VERSION until the session ends`
(for a warden from before renewals, which never said its version, the line
says it is of an earlier kind). Starting a new session is the way to get a new
warden. (Two builds of one
release say the same version, so for those `upgrade` cannot tell whether the
warden followed; `ps` shows `--resume` on the command line of a warden that
has renewed.)

Put each version in a directory of its own and leave the old one where it is.
Overwriting the binary of a running session in place costs the warden its
memory of where its relay comes from (see *Limits*).

## Security

- Sessions live in a directory only you can use: `/tmp/kablo-<uid>/`, mode
  0700, owned by you. Anything else is refused, not repaired. Every
  connection to a session's socket is checked for the uid of the process on
  the other end (`SO_PEERCRED`, `getpeereid`), and another user's is closed.
- kablo opens no network port on the server. Remote access is your ssh login
  running one command; whoever can log in as you can use your sessions, as
  they can use any other process of yours. (`kablo forward` listens on the
  machine it runs on, on the loopback only unless you ask otherwise.)
- A new session's program is an argument vector, never a shell string: nothing
  that arrives over the wire is joined into a command line, and the arguments
  and environment are not written to any log.
- The journal holds everything the program wrote, secrets it printed
  included. It is kept in memory (a `memfd` on Linux, anonymous shared memory
  on macOS), not in a file, up to `--journal-size` per session, and it goes
  when the session does.

## Limits

- **The picture is of a text screen.** A client that comes back gets the bytes
  it missed; if the journal has wrapped past them it gets a picture of the
  screen instead, and the stream from where the picture ends. The picture
  carries cells, colours and attributes, links, both screens, the scrollback
  (up to `--history` rows), the cursor and the modes. It does not carry
  images (kitty graphics) or the mouse mode that reports pixels, and the
  title comes back only if the program set one. The screen keeps the size of the last client that said one (once the
  program has ended it keeps the size it ended at).
- **A replaced or restarted relay rebuilds the screen from the journal**: from
  its start if nothing was dropped and the journal is not very large, else from
  the first line break in the last 64 MiB of it.
  A program that set modes or drew the screen long before that point (a
  full-screen editor entered hours ago, with output since) can come back
  without them until it draws again; the bytes a client gets are never wrong
  for it.
- **A picture is a screen, not the stream.** A client that takes it slower than
  the program writes (a slow link, a program printing without end) can be let go
  when the journal passes its picture, and is sent a newer one; one picture per
  attached client is held in the session until it is sent.
- **A terminal has at most 1000 columns and 1000 rows**: the session's screen
  keeps every cell of it, so a size is memory.
- **Questions to the terminal are answered by the session only while nobody is
  attached**: device attributes, the cursor position, mode and size reports, and
  whether the kitty keyboard protocol is there (answered as a terminal that has
  it would, as the client's own terminal does).
  With a client attached its own terminal answers, and a client that replays
  output it missed answers the questions asked meanwhile again. Colours, the
  clipboard and the size in pixels are never answered by the session.
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
  still prints `replaced`. The version in that line is the only hint: compare
  it with the one you installed.
- **A warden that cannot take the session up after it renewed** (a damaged
  binary, say) ends, and the session carries on with its relay alone: the shell
  and its programs live and the relay serves, but nothing restarts a relay that
  dies, nobody collects the shell's exit code, and `terminate` is refused.
  `kablo upgrade` cannot see this when the old and the new release are the
  same, and otherwise says that the warden stays.
- **The systemd scope** is tested against a stand-in `systemd-run`, not yet on
  a machine with a real systemd. The `setsid` route is what the end-to-end
  tests (a real sshd in a container) run.
- **Forwarding is local to remote and TCP only.** No `-R` (the server
  listening for you), no `-D` (SOCKS), no unix sockets, no UDP. One connection
  of `kablo attach` carries at most 128 connections open at once and connects
  16 at a time (one more is told "too many"); a target has 10 seconds to
  answer. `kablo forward` does not reconnect when its command ends.
- Windows and the BSDs are not supported, and a server with SELinux enforcing
  has not been tried (the ssh route asks sshd for no socket or port
  forwarding).

## For apps that drive kablo

- **The protocol** is in [docs/PROTOCOL.md](docs/PROTOCOL.md) and implemented
  by the `kablo-proto` crate, which does no I/O and has no dependencies. It
  also carries TCP connections (streams, on their own channel): an app asks
  `features` and, right behind it, `ping`, and relies on streams only if the
  answer to `features` comes first and says they are on. A
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
