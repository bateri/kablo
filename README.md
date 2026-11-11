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

kablo is the part of bateri that keeps terminals, and is not used anywhere
else. The terminal app drives it — locally over a unix socket, remotely over a
plain `ssh host kablo attach` — and kablo needs nothing from the server beyond
a login and the ability to run one command.

Status: early. Versions and what each one changed are in
[CHANGELOG.md](CHANGELOG.md); the binaries are on the
[releases page](https://github.com/bateri/kablo/releases). Linux (x86_64,
aarch64) and macOS (Apple silicon, Intel).

## Install

Download the binary for your machine from the
[latest release](https://github.com/bateri/kablo/releases/latest) (until the
first one is published, the page is empty: build from source, below). Its name
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

The macOS binaries are signed with a Developer ID and notarized by Apple. One
a browser saved opens without a prompt (the Mac asks Apple about it once, so
the first run wants the network: a bare executable cannot carry its ticket);
one fetched with `curl` or copied over ssh carries no quarantine mark and is
not asked about at all, so a Mac you reach only over ssh takes it as it is.

To build it from source instead, with Rust 1.88 or newer:

```sh
cargo build --release          # target/release/kablo
```

`make dist` builds the files a release ships: `dist/kablo-<version>-<target>`
for the four targets, with `LICENSE`, `THIRD-PARTY-LICENSES.txt` and the
`SHA256SUMS` over all of them. The build is named for the commit it was made
from, `<version>-dev+<commit>` (and `.dirty-<digest>` for a working tree that is
not that commit), in the file names and in what `kablo version` says; only
`make release` names a build by its version alone, so two builds never share a
name and a server's directory per version never holds two different files. It needs a Mac with Docker (the Linux binaries
are static musl builds, made and tried in a container of their own
architecture), a local `rustc` of the version named in
`tools/linux/Dockerfile` (as for `make linux`), and rustup with both Apple
targets installed in its `stable` toolchain (`rustup target add
aarch64-apple-darwin x86_64-apple-darwin --toolchain stable`; Homebrew's Rust
cannot add them). The macOS binaries are built by that toolchain, the Linux
ones by the compiler in the image. `make release` runs every gate and then
builds the same files with the macOS binaries signed and notarized
(`make dist-signed`: a Developer ID Application identity in the keychain and
a `notarytool` profile, `NOTARY_PROFILE`) into `target/release-<version>/`,
and `make ship` publishes the
result as a GitHub release; a version is released only if `CHANGELOG.md` has
a section for it.

The binary is one file. Put it where your shell finds it, on a server too. The
program a terminal app runs over ssh is `kablo attach`, and ssh runs it in a
non-interactive shell, whose `PATH` often leaves out `~/.local/bin`; apps
therefore give ssh the path itself
(`ssh host .local/share/kablo/0.1.0/kablo attach`, see *For apps that drive
kablo*).

## How bateri uses it

bateri starts each of its terminals as a kablo session (`kablo new`), attaches
to it over the unix socket of the machine, or over `kablo attach` on a server,
and puts the terminal's program in its panes. Programs in a pane reach the
others through `kablo bateri VERB`, which the app puts on the `PATH` of the
terminals it opens (`kablo bateri --help`). The commands that start a session
and drive one without attaching to it (`new`, `list`, `send`, `send-key`,
`capture`, `wait`, `label`, `end`, `here`) are what `kablo bateri` is made of
and what the tests start sessions with; they are not a user interface and are
not described here.

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

The same connection carries programs and TCP connections for the app, and is
how a program on the server reaches it: `kablo bateri` there hands its request to
the app that is connected through `kablo attach` and brings the answer back, from
inside a session or outside every one, with nothing to set up on the ssh side.

What a terminal app does with that is its own business; the protocol is in
[docs/PROTOCOL.md](docs/PROTOCOL.md).

## Replacing kablo under running sessions

Each session is two processes: a *warden*, which is the shell's parent, and a
*relay*, which listens for clients, reads the terminal and keeps the journal.
An upgrade replaces both, without ending the shell and without dropping a
client:

```sh
kablo upgrade ID /path/to/new/kablo
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
- A running `kablo attach` keeps a mailbox in that same directory, `mail-<pid>`,
  a socket on which programs of yours on the machine can ask where the app
  connected through that bridge stands (which terminal it is at, and how long
  ago it last used it) and hand it bytes, which kablo carries without looking at
  them. It is as private as a session's: the same directory, and a connection
  from another user's process is closed. The terminal an answer or a hand-over
  names is what the kernel says of the process that asked, never anything that
  process claims. What the app does with the bytes is up to the app; kablo
  hands over nothing to an app that has not said it takes them. The mailbox goes
  when the bridge does, and with it every conversation in it.
- kablo opens no network port on the server. Remote access is your ssh login
  running one command; whoever can log in as you can use your sessions, as
  they can use any other process of yours.
- A new session's program is an argument vector, never a shell string: nothing
  that arrives over the wire is joined into a command line, and the arguments
  and environment are not written to any log.
- The journal holds everything the program wrote, secrets it printed
  included. It is kept in memory (a `memfd` on Linux, anonymous shared memory
  on macOS), not in a file, up to a limit per session, and it goes
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
  With a client attached its own terminal answers. A session that was started to
  wait for its first client (`kablo new --wait-for-client`, at most ten seconds)
  answers nothing until that client has attached or the wait has passed: a
  program that asks the moment it starts is then answered by the terminal it was
  started for. A client that comes back
  is not left to answer them again: an app that cannot hold its answers back gets a picture of the screen
  in place of bytes that hold a question the session answered, and an app that
  has a terminal of its own is told how far the questions were answered (see
  below). A client that was attached when a question was asked, and was lost
  before the question reached its terminal, leaves the question unanswered, as
  a plain terminal would have. Colours, the clipboard
  and the size in pixels are never answered by the session.
- **A program that does not read its input** gets only so much of it. The
  session holds 1 MiB of typed bytes for it; past that, an `input` is not
  written and its sender is told (`INPUT_FULL` in the protocol; through the
  bridge it reaches the client unchanged). Everything else about the session
  keeps working: `status`, `list`, `resize`, `detach` and `terminate` are
  answered, and its output goes on flowing. The refused bytes are not queued
  anywhere, so send them again once the program reads.
- **Every `wait` holds one of a relay's 64 client places** while it waits, so
  very many of them can keep an app from connecting to the session.
- **The keys `send-key` sends are as right as the relay's idea of the
  terminal's modes.** A relay that was replaced or restarted rebuilds its screen
  from the journal (see above), and until the program sets its modes again they
  can be approximate: `send-key up` can briefly send the sequence for the other
  cursor mode.
- **A `wait` that starts after a client has been told the program ended** can
  find no session (255): that client took the session over and it is gone. Getting
  the program's status for certain is what a `wait` that was already waiting is
  for. An app can ask for the session to be kept for ten
  minutes after it has been told (below, For apps), and a `wait` or a `capture`
  that comes in that time finds it.
- **A label is a file in the socket directory.** A system that clears `/tmp` by
  age can remove it; the session then has no label, which is not an error.
- **A program that ignores the hang-up** is ended by `kablo end --force`, which
  kills it. One that even that does not end (stuck in the kernel, say), or a
  `broken` session, is ended with `kill`: find the keeper in `ps` (`kablo warden
  --id ID`) and stop it and the program under it. Apps end a session with the
  protocol's `terminate`, which is what `kablo end` sends.
- **Input typed at the instant a relay is replaced** (`kablo upgrade`) can be
  lost; output is not.
- **The journal is bounded** (8 MiB by default). Output the program writes
  while nobody is attached is kept only as far as that.
- **A bridge from before taking the terminal** (a `kablo attach` that was running
  when kablo was upgraded, or one of an older kablo on a server whose sessions
  were upgraded) passes the `TAKEN` error on to its client but remembers the
  client's attach, so when its relay is replaced or restarted it attaches the
  client again beside the one that took it. It does not carry the name or the take
  bit of an attach on either: an app attaching through it is neither named in
  `status` nor takes the terminal. Starting the bridge again (the app connecting
  anew) ends this.
- **A take is done by the relay that hears it.** If that relay is replaced or dies
  at that moment, a client that was to be let go may be attached again beside the
  one that took, and not be told. An app that needs the terminal to be its own reads
  `status` (key 21) after a reconnect.
- **An ended session** is kept until a client has been told how it ended, a
  day at most, and then removed; for ten minutes more, counted from the telling,
  when that client asked for it (an app does, so that a program on the server can
  still read the terminal that the app shows as ended). Those ten minutes are not
  kept if the session's relay dies and is started again meanwhile: the session is
  then kept until an app that asks is told again, or for the day.
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
  answer. An administrator turns connections off with
  `/etc/kablo/no-forwarding`; programs are not connections and still run.
- **A command is a program, not a session.** It has no terminal, it is not
  kept when the connection ends, and nothing of what it wrote is kept for a
  client that comes back. It counts among the 128 streams of a connection and
  its output and errors share one window, as large as the link needs and at
  most 2 MiB, as an ssh channel has.
- Windows and the BSDs are not supported, and a server with SELinux enforcing
  has not been tried (the ssh route asks sshd for no socket or port
  forwarding).

## For apps that drive kablo

- **The protocol** is in [docs/PROTOCOL.md](docs/PROTOCOL.md) and implemented
  by the `kablo-proto` crate, which does no I/O and has no dependencies. It
  also carries TCP connections and programs (streams, on their own channel):
  an app asks `features` and, right behind it, `ping`, and relies on streams
  only if the answer to `features` comes first and says they are on, and on
  commands only if it says so too (`commands`, independent of the streams). A
  build reads its own wire version and the one before it, and the bytes of
  every message of every shipped version are kept as test fixtures.
  `kablo version` prints one line of `key=value` tokens
  (`version=… wire_min=… wire_max=…`): keys may be added to it, none removed.
- **Driving a session without attaching** (typing text and keys, reading the
  screen as text or as a picture, waiting for the program to end, a label) is
  messages of the session channel, which neither attach the sender nor change
  what the session answers its program (section 5 of
  [docs/PROTOCOL.md](docs/PROTOCOL.md)). A relay from kablo 0.2.0 on takes them;
  an older one skips what it does not know, so an app reads `version` in
  `status` before it relies on them. `status` and `new` carry the label, and
  `list --json` has the key `label`; `status` and `list --json` also carry when
  the session started, when the program in front did, whether that program is
  the session's own and when the program last wrote. `kablo wait`
  ends with the program's status, 124 when `--for` ran out and 255
  for kablo's own trouble; the other commands end with `0`, `1` for kablo's own
  trouble and `64` for a command line that does not parse.
- **A terminal that ended stays readable for a while** when the app that shows it
  asks: bit 2 of the view it attaches with (`keep_after_end` in `kablo-proto`).
  Once the app has been told the program ended, `capture`, `wait`, `status` and
  `list` go on answering for ten minutes, for a program on the server that comes
  after the app (an agent that closes or reads the terminal the app opened for
  it); a `write` is refused and an `input` is dropped, as for any ended session.
  The app does not need to stay attached. A relay
  from before kablo 0.2.0 ignores the bit and removes the session when the app
  is told, as it always did.
- **Mail from programs on the server** goes to the app through the bridge it
  already runs: a program on the server (`kablo bateri` is one) hands its bytes to
  the app that is connected and carries the app's back, under a ticket that names
  the terminal the program runs in, without reading either side (section 5 of
  [docs/PROTOCOL.md](docs/PROTOCOL.md)). An app says it takes tickets in its
  `features`, and finds out whether a bridge has a mailbox by `mailbox` in the
  answer. With no app that takes mail (the app has not said so, or the bridge is
  from a kablo that had no mailbox) the sender has none to hand it to.
- **`kablo bateri VERB`** is the command a terminal app puts on the `PATH` of the
  terminals it opens, for the programs (a coding agent, a build script) that run in
  them: `split`, `tab` and `window` start a terminal and ask the app where its pane
  goes, and `send`, `send-key`, `capture`, `wait`, `ls`, `notify`, `message`, `focus`, `zoom`,
  `swap`, `rename`, `close`, `move` and `resize` drive the others (`kablo bateri
  --help`). What needs the app's windows is one text message of the `kablo-bateri`
  crate, which the app reads with the same code, over the mailbox above; the
  terminals themselves are kablo's. A pane of another machine is written `p9@prod`, and
  `-S prod` starts a terminal there: the app says how that machine is reached, and
  the command runs there over the connection the app keeps. `kablo bateri
  --next-number` gives the next terminal number of the machine (the app's own
  bootstrap and the command count with the same file, so no number is given twice),
  and the exit status is `0`, `1`, `64` (a mistake in the command), `69` (no app) or
  `77` (the app refused).
- **Taking a terminal** is bit 3 of the view an app attaches with (`take` in
  `kablo-proto`): the other clients attached to the session are let go, each with
  the error `TAKEN` (code 9) whose text is the name the app gave itself in its
  view, and nothing more of the session comes to them; their connections stay, and
  a bridge goes on serving the app's other terminals. An app that is let go shows
  its terminal as no longer its own and may attach again, with the bit or without
  it. `status` says who is attached (the names, key 21) before and after. A relay
  from before the bit attaches the app beside the others, which `status` shows.
- **The terminal's state and its answers** are told, not asked for. Every
  attached client is sent a `state` right after `attached` and again when it
  changes: the program in the foreground and its directory, whether the terminal
  is asking for a password, and where an interactive `ssh` or `mosh` goes (the
  target only, never the rest of its command line). `attached` also says how
  far the session answered the program's questions, so an app with a terminal
  of its own that is shown output again can leave those questions unanswered.
  Both are in [docs/PROTOCOL.md](docs/PROTOCOL.md).
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
