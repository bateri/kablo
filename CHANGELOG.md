# Changelog

All notable changes to kablo are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and versions follow
[Semantic Versioning](https://semver.org/).

Write user-facing changes under **Unreleased** as they land: what someone
running kablo will notice, not how the code changed. At release time rename
the section to the new version and date. `make release` takes that section as
the release notes, which are the text of the GitHub release. A version without
a section is not released.

## [Unreleased]

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
- **Upgrades under running sessions.** `kablo upgrade ID /path/to/new/kablo`
  (or `--all`) replaces a session's relay and then its warden, the process
  that keeps the shell, in place: the shell keeps running and an attached
  client is reconnected without noticing. Fixes to the warden reach sessions
  that are already running, not only new ones.
- **Binaries for Linux (x86_64, aarch64) and macOS (Apple silicon, Intel).**
  The Linux ones are static, so they run on any distribution. Every release
  lists the binaries' checksums in `SHA256SUMS` and carries the license
  notice for the third-party code inside them.

### Known limits

- The macOS binaries are not signed or notarized. A file a browser saved
  carries macOS's quarantine mark and Gatekeeper refuses to run it until you
  remove the mark (`xattr -d com.apple.quarantine <file>`); a file fetched
  with `curl` carries none.
- The screen picture is of a text screen: it does not carry images, and the
  rest of what a returning client does not get back is in the README, under
  [Limits](https://github.com/bateri/kablo#limits).
- Windows and the BSDs are not supported.
