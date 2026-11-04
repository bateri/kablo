# This is a fork of alacritty_terminal 0.26.0

`alacritty_terminal` is the terminal emulator kablo's relay keeps a screen with and bateri
runs its panes on. This directory is the one copy of the fork: kablo's workspace points
cargo at it with `[patch.crates-io]` in the root `Cargo.toml`, and bateri takes it from this
repository by a pinned commit (`rev`) in its own `[patch.crates-io]`, next to the one for
`vte` (`vendor/vte`, its sibling fork). The name and version are upstream's (0.26.0), so
every dependent resolves to the fork without knowing it.

The files are the `alacritty_terminal` 0.26.0 package from crates.io (Apache-2.0), without
`.cargo-ok` and `Cargo.toml.orig`, and with a `.gitignore` of ours (`target`). The diff between the package and this
directory is the list of changes below. The license is Apache-2.0 only,
so section 4(b) applies — every file a change touches says so in a notice at its top, and the
list below names the file.

The fork stays a soft one: it only **adds** (one fix upstream made after the release aside,
listed below). It does not touch the cell, the grid's storage
or the parser's behavior, and `alacritty_terminal`'s public API keeps its shape, so moving to
a newer upstream release is a merge of additions and not a port. Something that would change
those (a cell of our own, a different storage) is a different decision, and a different
crate name.

The fork is not a member of kablo's cargo workspace (`exclude` in the root `Cargo.toml`,
which cargo also needs to find the package inside a git dependency), so `fmt` and `clippy`
leave upstream's formatting and lints alone. Its tests, upstream's and ours, run in
`make check` and `make linux`: as the package it is (`cargo test --manifest-path
vendor/alacritty_terminal/Cargo.toml --all-targets --locked`, since cargo will not test a
non-member that has dev-dependencies as `-p` of the workspace), on the `Cargo.lock` upstream
shipped with it (but for its `vte`, which is the fork next to this one: see the last change
below) and with its default features, so upstream's reference tests run too.

## Changes

- **The primary screen can be read while the alternate screen covers it.**
  `Term::hidden_primary_grid() -> Option<&Grid<Cell>>` returns the grid of the primary
  screen, read only, when a program has taken the alternate screen (`ALT_SCREEN`), and `None`
  otherwise. Upstream keeps that grid in the private `inactive_grid` and gives no way to look
  at it, so what the shell printed before a full screen program started (the rows and the
  history behind them) could not be read until the program ended. Nothing else changes: the
  grid is not copied, swapped or touched, and the accessor is a plain borrow. Touches
  `src/term/mod.rs`, which carries the §4(b) notice. Test:
  `the_primary_screen_stays_readable_behind_the_alternate_one` in the same file.

- **A full keyboard mode stack drops its own bottom entry.** In 0.26.0 the push that finds
  the kitty keyboard mode stack at its limit (`KEYBOARD_MODE_STACK_MAX_DEPTH`, 4096) removes
  the bottom entry of the **title** stack instead: with no title pushed that is a panic on
  the thread that parses the program's output, so any program could end the terminal by
  pushing a mode 4097 times; with titles pushed one is lost and the keyboard stack grows
  without limit. Upstream fixed it after the release with the same one-word change
  (alacritty `542147067d`, "Fix Kitty keyboard mode stack size limit"), so the next release's
  merge drops this entry. It is the one change here that is not an addition, taken because
  the bug sits in a path every dependent runs once `Config::kitty_keyboard` is on. Touches
  `src/term/mod.rs`. Test: `a_full_keyboard_mode_stack_drops_its_own_bottom_entry` in the
  same file.

- **A run of printable ASCII is written in one go.** `Term` implements
  `ansi::Handler::input_ascii(&str)`, the method the `vte` fork added (`vendor/vte`, its
  `FORK.md`; upstream's `vte` has no such method, so this fork no longer builds against the
  registry's). It writes a run into the cells it lands on stretch by stretch, where `input`
  looks up the width, maps the charset, copies the template and checks for fullwidth cells for
  each character. The result is the one `input` for each character leaves, always: the cells,
  the cursor and `input_needs_wrap`, the rows' `occ` and the damage. Anything that makes a
  character depend on more than its own cell is left to `input`: insert mode, a charset other
  than ASCII, line wrap off, a template with `extra` (a hyperlink, an underline color), and a
  target cell that is a fullwidth character, its spacer or holds `extra` (`write_at_cursor`
  clears the other half of the character and the previous line's leading spacer, and
  replaces `extra`). The cell, the grid's storage and the parser are not touched. Touches
  `src/term/mod.rs` (§4(b) notice) and adds `src/term/input_ascii_tests.rs` (notice at its top),
  whose differential test runs the same random mix of text, wide and zero-width characters,
  attributes, hyperlinks, modes, charsets, regions, the alternate screen, cursor moves and REP
  through a `Term` taking runs and one driven character by character, and compares every cell,
  the cursor, `occ`, the damage and the modes after each read. A bump must carry both and
  the tests; `make check` runs this package's tests against the `vte` fork (the Makefile's
  `--config` with the `[patch.crates-io]` the package is not part of, and the `vte` entry of
  `Cargo.lock` is the path instead of the registry).

- **The rows a scroll will recycle are asked for ahead of their turn.** With a full history
  every scroll resets the oldest row, whose cells nothing has touched for as many scrolls as
  the history has lines, so they are in memory and not in any cache, and `Row::reset` is
  mostly waiting for them (on plain text it was over half of what `Term` cost). `Grid::scroll_up`
  now also calls the new `Row::prefetch_for_reset` on the lines that will come up 32 scrolls
  later, which issues the CPU's prefetch hint (`prfm` on aarch64, `prefetcht0` on x86_64,
  nothing elsewhere) for the cache lines of their occupied cells. Nothing is read or written as
  far as the program can tell: the cell, the storage and every result are as they were. Touches
  `src/grid/mod.rs`, `src/grid/row.rs` and `src/grid/tests.rs` (all carry the §4(b) notice).
  Test: `prefetching_for_a_reset_changes_nothing`.

## When this directory changes

bateri pins this repository's commit, so a commit that changes the fork reaches bateri only
when bateri's `rev` (in its `[patch.crates-io]`) is moved to it — the commit must be pushed
first, and bateri's `make check` and `make linux` run against the new `rev` before it is
committed there.

## When upstream releases a newer `alacritty_terminal`

Take the new version's package from crates.io, add it as a commit of its own so the diff to
the previous one is readable, then bring each change above forward (`diff -r` between the
package and this directory is the list) and bump the version in
`vendor/alacritty_terminal/Cargo.toml`, in the `=` pins of both repositories and in the
comments next to `[patch.crates-io]`. The new release asks for a `vte` of its own: that is
`vendor/vte`'s turn first (its `FORK.md`). Then move bateri's `rev` to the new commit.
