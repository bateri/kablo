# Changelog

All notable changes to kablo are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and versions follow
[Semantic Versioning](https://semver.org/).

kablo has two kinds of reader, and this file is for both:

- **People who run kablo.** Write what they will notice, not how the code
  changed: the commands, what they print and how they exit, what a session
  does, what the README promises. Under **Added**, **Changed**, **Fixed** and
  **Removed**.
- **Apps that drive kablo**: a terminal app over `kablo attach`, a program
  built on the `kablo-proto` crate. They pin a version, install it on servers
  and read this file when they move to the next one, so every change to what
  they rely on is also written under **For apps**: the protocol
  (`docs/PROTOCOL.md`), the public API of `kablo-proto`, the keys of
  `kablo list --json` and `kablo version`, the exit codes, the names and
  targets of the release files and where `kablo_proto::install` puts them,
  and the vte fork in `vendor/vte` (its changes are listed in its `FORK.md`).

What apps rely on only grows. A message, a key, a field or a code that has
been released keeps its meaning, and a new need is a new one. Something on its
way out stays for at least one release and is listed under **Going**, with
what replaces it. A change to the protocol's format is a new protocol version,
read alongside the one before it (`docs/PROTOCOL.md`, Compatibility).

Write a change under **Unreleased** in the commit that makes it, not on the
day of the release. At release time rename the section to the new version and
date, above a new empty Unreleased. `make release` takes that section as the
release notes, which are the text of the GitHub release. A version without a
section is not released.

## [Unreleased]

### Added

- **`kablo app`: hand bytes to the app that is connected.** A program on a
  server (an agent in a terminal, a script, a cron job) can send bytes to the
  app that reaches that server through `kablo attach` and read bytes back:
  `kablo app` joins its standard input and output to one conversation with the
  app. kablo carries the bytes without reading them and tells the app which
  session the program runs in, or that it runs in none; what they mean is
  between the program and the app. It goes to the app that was last used at the
  program's session, or else the one last used anywhere on the machine. It ends
  with `0` when the conversation ended, `64` when it is given an argument (it
  takes none), `69` when no app that takes these conversations is connected, `1`
  for anything else that went wrong (each says what on standard error), 128 plus
  the signal's number after a signal, and `141` when whatever reads its output
  goes away. A bridge that is running when kablo is upgraded keeps no mailbox
  until its app connects again, which the message for `69` says (README, Talking
  to the app). To be reachable, each `kablo attach` now listens on a socket of
  its own in the socket directory, which it makes if it is not there, for as
  long as it runs; only the same user connects to it (README, Security).

### For apps

- **Protocol: mail.** A program on the server can hand bytes to the app that is
  connected, and get bytes back, with kablo carrying them unread and saying which
  terminal they come from (`docs/PROTOCOL.md`, section 5, Mail). Added to
  version 1 and nothing else changed: on the session channel `ask` (0x0c) and
  `decline` (0x0d) from the client, `ask_reply` (0x8c) and `ticket` (0x8d) from
  the bridge; on the stream channel an `open` key 6 (`ticket`) and failure code 9
  (the app declined); in `features` an optional body (key 1, `tickets`: the app
  takes tickets) and in `features_reply` key 7 (`mailbox`). A reader that does
  not know them skips them, so an older bridge or app is unaffected and an app
  tells an older bridge from one with a mailbox by `mailbox` being absent. The
  sender's side is the command `kablo app` (below).
- **`kablo app`: the sender, and what an app can rely on of it.** It asks the
  mailbox of every bridge on the machine (`ask`), keeps the bridges whose client
  takes tickets (`tickets` in `ask_reply`), and opens the conversation on the one
  attached to the terminal it runs in that was used last (`idle` least), or else
  the one used last of all; an app is `attached` to a terminal by attaching to it
  and is used by attaching, typing or connecting, so the app that connected again
  after a lost connection is chosen over the half-dead bridge it left. A
  conversation is a stream: the sender's input is `data` and then `eof`, the
  app's output is `data`, and the app's `close` ends it (what the app sent before
  is still written out); both ending with `eof` ends it as well. The sender waits
  5 seconds for a bridge to answer `ask` and 10 for the app to answer a ticket
  (it then sends `close`), and nowhere else; a bridge that does not answer is
  left out. Exit codes: `0` ended, `69` no app that takes tickets is connected
  (no mailbox, no client that says `tickets`, or the bridge's `failed` code 2
  with no client to give a ticket to), `64` an argument was given, `1` the app
  declined (code 9), did not answer, went away without `close`, the bridge
  refused for another reason (`failed` code 5 or 8: a window or a number of
  streams over a limit, tickets used up, the terminal could not be looked for) or
  the conversation could not be written, and 128 plus the signal's number when it
  was ended by `SIGINT`, `SIGTERM` or `SIGHUP` (141 for `SIGPIPE`: whatever reads
  its output went away).
- **`kablo-proto`: the API for it.** `v1::Message` gains `FeaturesWith(Offers)`
  (a `features` with a body; `Features` stays the one without, and is also what a
  body that cannot be read reads as), `Ask`, `Decline`, `AskReply(Standing)` and
  `Ticket`; `v1::stream::Message` gains `OpenTicket` and the constructor `open_mail`;
  `v1::stream::FailCode::DECLINED` is 9; `v1::Features` gains the field `mailbox`
  (build it with `Features::with_mailbox`). `v1::kind` gains `ASK`, `DECLINE`,
  `ASK_REPLY` and `TICKET`. `Offers` and `Standing` are `#[non_exhaustive]` and
  made with `Offers::with_tickets` and `Standing::idle` (then
  `Standing::with_attached` and `Standing::with_tickets`); both have `to_block`
  and `from_block`.
  The bridge keeps a mailbox (`mail-<pid>` in the socket directory, made if the
  directory is not there), answers `ask` on it, says `mailbox` in
  `features_reply`, and gives a client that has said it takes tickets a `ticket`
  for each stream a program opens there; the client's `open` that answers it
  joins the two streams, and the bridge passes `data`, `credit`, `eof` and
  `close` between them under the windows the two ends gave (an end that breaks
  one loses the stream: both are sent `close`). The client going away ends the
  bridge and with it every connection to the mailbox, with no `close` on the
  streams that were open.
- **Changed, `kablo-proto`: breaking for code that matches `Message`, and for code
  that builds `Features`, `Status` or `NewSession`.** `v1::Message` and
  `v1::stream::Message` are not `#[non_exhaustive]` and have the variants above
  now, so a `match` on them with no catch-all arm no longer compiles; and a
  `features` that has a body reads as `FeaturesWith`, so a server that answers
  only `Message::Features` does not answer it.
- **Changed, `kablo-proto`: `Features`, `Status` and `NewSession` are
  `#[non_exhaustive]`.** Code outside the crate could write them as struct
  expressions in 0.1.0 (`Features { streams: true, ..Features::default() }`) and
  can no longer. Make one with its constructor and set the fields, which stay
  public: `Features::with_streams()`, `Features::streams_off(why)` or
  `Features::default()`; `Status::new(id)`; `NewSession::default()`. Reading is
  as before, except that a pattern naming the fields of one ends in `..`. This is
  so that a field added to them later, as `Features::mailbox` is in this release,
  breaks no app that builds them. The wire format is not touched.

## [0.1.0] - 2026-10-15

The first release. kablo holds the terminal of a shell so the programs in it
outlive the connection that started them, and draws nothing of its own: a
terminal app puts the windows on top.

### Added

- **Sessions that outlive the connection.** `kablo new` starts your shell (or
  the program you name) in a session that goes on without you; `kablo connect`
  picks it up from any terminal. Close the laptop, lose the network, restart
  the terminal app, and the programs keep running. While you are attached, the
  bytes a program writes reach you unchanged.
- **One ssh connection carries every session.** `ssh host kablo attach` speaks
  kablo's protocol on its standard input and output, so a terminal app can
  list, start and attach any number of sessions over one login, and sshd's
  limit on sessions per connection never comes into it. The protocol is in
  `docs/PROTOCOL.md`.
- **A broken connection continues from the byte it stopped at.** A client that
  comes back asks for the bytes after the last one it received and gets
  exactly those. If a session's relay dies or is replaced while a client is
  attached, `attach` connects again and the stream goes on without a gap or a
  repeat.
- **A picture of the screen when the journal has wrapped.** A client that was
  away longer than the journal reaches (8 MiB by default, `--journal-size`)
  gets a picture of the session's screen first (cells, colours, attributes,
  links, both screens, the scrollback, the cursor and the modes), then the
  program's output as it comes.
- **Programs get their answers while nobody is attached.** Device attributes,
  the cursor position and mode and size reports are answered by the session
  itself, so a program that asks its terminal something does not stall until a
  client returns.
- **A client can show what runs in the terminal.** Every attached client is
  told which program is in front and its directory, whether the terminal is
  asking for a password, and where an interactive `ssh` or `mosh` in front
  connects to (the target and nothing else of its command line). It is told
  when that changes, and the session looks only after output or typing, never
  on a timer while nothing happens. The protocol's `status` carries the same,
  and so does `kablo list --json` (`secret`, `remote`, `remote_port`).
- **A program is not answered twice when you come back.** A question that was
  answered already (where the cursor is, what the terminal is) used to be
  answered again by your terminal when the output was shown again, and the
  second answer was typed into the program. `kablo connect` is now shown a
  picture of the screen in place of such output, and an app with a terminal of
  its own is told how far the questions were answered.
- **Ports through kablo's own connection.** `kablo forward 8080:localhost:3000
  -- ssh devbox kablo attach` does what `ssh -L` does, over the one connection
  of `kablo attach`, so it works where sshd has forwarding turned off. Only
  the loopback listens unless you pass `--bind`, and a large transfer never
  holds up typing and replies. An administrator who does not want connections
  carried creates `/etc/kablo/no-forwarding`; kablo then refuses them, on
  running bridges too.
- **Programs run on the server over the same connection.** `kablo exec
  -- ssh devbox kablo attach -- tar -cf - project` does what `ssh devbox tar
  -cf - project` does, without asking ssh for another channel: the program has
  no terminal, its input comes from here, its output and errors come back
  apart, and `kablo exec` ends with its exit status (255 when kablo itself
  could not run it). The program ends with the connection, or when `kablo
  exec` is interrupted. Apps that speak the protocol get the same through an
  `open` that names a program, and learn whether a server can from `features`.
  An administrator's `/etc/kablo/no-forwarding` does not touch programs.
- **Transfers as fast as the link, and typing that does not wait behind
  them.** A forwarded connection or a program's input and output may have up
  to as much on its way as an ssh channel (2 MiB), so a server far away is not
  slower through kablo; and no more than the link needs, which kablo measures
  as the transfer goes, so a slow link is not filled with a queue that what
  you type then waits behind: over a link of 1 MB/s, typing during a transfer
  echoes in about a third of a second where a fixed 2 MiB made it two.
- **Upgrades under running sessions.** `kablo upgrade ID /path/to/new/kablo`
  (or `--all`) replaces a session's relay and then its warden, the process
  that keeps the shell, in place: the shell keeps running and an attached
  client is reconnected without noticing. Fixes to the warden reach sessions
  that are already running, not only new ones.
- **Binaries for Linux (x86_64, aarch64) and macOS (Apple silicon, Intel).**
  The Linux ones are static, so they run on any distribution; the macOS ones
  are signed with a Developer ID and notarized by Apple, so a Mac opens them
  without a prompt, and over ssh they need no desktop session. Every release
  lists the binaries' checksums in `SHA256SUMS` and carries the license
  notice for the third-party code inside them.

### Known limits

- A macOS binary a browser saved asks Apple about itself the first time it
  runs, so that first run needs the network (a bare executable cannot carry
  its notarization ticket). One fetched with `curl` or copied over ssh is not
  asked about.
- The screen picture is of a text screen: it does not carry images, and the
  rest of what a returning client does not get back is in the README, under
  [Limits](https://github.com/bateri/kablo#limits).
- Windows and the BSDs are not supported.
