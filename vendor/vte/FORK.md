# This is a fork of vte 0.15.0

`vte` is the terminal parser inside `alacritty_terminal`. This directory is the one
copy of the fork: kablo's relay and bateri both parse with it, so "the same parser" means
the same source. kablo's workspace points cargo at it with `[patch.crates-io]` in the
root `Cargo.toml`; bateri takes it from this repository by a pinned commit (`rev`) in its
own `[patch.crates-io]`. The name and version are upstream's (0.15.0), so
`alacritty_terminal` resolves to the fork without knowing it.

The files are the `vte` 0.15.0 package from crates.io (Apache-2.0 OR MIT; both license
files are here), without `.cargo-ok`, `Cargo.lock`, `Cargo.toml.orig` and the upstream CI
description. The fork lived in bateri's repository before it moved here; the diff between
the package and this directory is the list of changes below. Upstream's code keeps
upstream's formatting: this directory is not a member of kablo's cargo workspace
(`exclude` in the root `Cargo.toml`, which cargo also needs to find the package inside a
git dependency), so `fmt` and `clippy` leave it alone. Its tests, upstream's and ours,
run in `make check` and `make linux` (`cargo test --manifest-path vendor/vte/Cargo.toml
--all-targets --features ansi` — `ansi` is the feature `alacritty_terminal` turns on, and
most of the changes below live behind it).

## Changes

- **A character cut by a read keeps the bytes after it.** `Parser::advance_partial_utf8`
  completes a character cut at the end of one `advance` by copying up to three bytes of
  the next into a buffer. When the buffer held the rest of that character, more complete
  characters and the start of one that was itself cut, it printed the first character
  but reported every valid byte of the buffer as consumed, so whatever stood between
  the characters was never printed (`C2 | B7 20 C2 B7` printed `··`, not `· ·`). It now
  reports only the first character's bytes; the rest is parsed from the input as usual.
  Tests: `partial_utf8_keeps_the_bytes_after_the_character` and
  `utf8_cut_anywhere_dispatches_what_the_whole_does` (every one- and two-way cut of a
  text mixing one- to four-byte characters, ASCII and an escape sequence).

- **Application program commands reach the handler.** Upstream parses `ESC _ … ESC \`
  into a state that throws every byte away. `Perform` and `ansi::Handler` now get
  `apc_start`, `apc_put(&[u8])` and `apc_end(cancelled)`, empty by default, so
  `alacritty_terminal` compiles unchanged. The body arrives as the slices the reads made
  (found with `memchr`, never buffered here: the handler decides how much it keeps);
  bytes are not interpreted, only `ESC`, `CAN` and `SUB` end it. `ESC \` completes the
  command (`cancelled` is `false`); `CAN`, `SUB` and an `ESC` followed by anything else
  cancel it, and that byte is then parsed as the one after an `ESC` that starts afresh.
  SOS and PM (`ESC X`, `ESC ^`) are still dropped. Unlike DCS, `0x9C` is no terminator:
  the state machine takes no 8-bit controls and a UTF-8 body may hold the byte.
- **The parser can halt right after a command.** `Handler::halt_requested` (false by
  default) is asked after every byte; while it is true `Processor::advance_until_halt`
  returns the number of bytes it took, and `Processor::advance` (unchanged) never asks.
  The handler owns the request: it is raised in `apc_end` and the caller takes it back
  after doing what it wanted outside the parser (decoding an image without the terminal's
  lock). A halted processor takes no byte until then. A synchronized update's replay
  halts too: its remaining bytes wait in the sync buffer (`Replay`) and the next call
  resumes them before any new byte, empty slice included, and the update is reported over
  only after the last pause. `Processor::stop_sync_until_halt` is `stop_sync` that can
  halt. Tests: the `apc_*`, `sos_and_pm_are_dropped` tests in `lib.rs` and the
  `*halt*` tests in `ansi.rs` (every one- and two-way cut of streams with commands, with
  and without a synchronized update, the 2 MiB overflow, a timed-out update).
- **The pointer shape request (OSC 22) reaches the handler as written.** Upstream reads
  the payload as one CSS name and drops anything else, so kitty's protocol (an empty
  request resets the shape, `>` and `<` push and pop, `?` queries, a comma-separated list)
  never got through. `ansi::Handler::set_pointer_shape(request, terminator)` now gets the
  payload (empty for `OSC 22 ST` too) and the terminator a reply takes; its default body
  is upstream's reading, so `alacritty_terminal` behaves as before. Tests:
  `pointer_shape_request_reaches_the_handler_as_written` and
  `pointer_shape_name_is_set_by_default` in `ansi.rs`.

## When this directory changes

bateri pins this repository's commit, so a commit that changes the fork reaches bateri
only when bateri's `rev` (in its `[patch.crates-io]`) is moved to it — the commit must be
pushed first, and bateri's `make check` and `make linux` run against the new `rev`
before it is committed there.

## When `alacritty_terminal` asks for a newer `vte`

bateri's `make audit` goes red (cargo only warns that the patch is unused, then builds the
registry's `vte`, so the changes above would silently disappear). Take the new
version's package from crates.io, add it as a commit of its own so the diff to the
previous one is readable, then bring each change above forward and bump the version in
`vendor/vte/Cargo.toml` and in the comments next to `[patch.crates-io]` in both
repositories. `diff -r` between the package and this directory is the list of changes.
Then move bateri's `rev` to the new commit.
