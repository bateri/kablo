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

- **Drive a session from the machine it runs on, without attaching to it:
  `kablo send`, `send-key`, `capture`, `wait` and `label`.** A program there
  (an agent, a script) types text and keys into a session, reads its screen,
  waits for its program to end and names the session, while the session goes on
  answering what its program asks its terminal and an app that shows it is not
  disturbed. `kablo send [ID] [TEXT | -]` types text without pressing Enter
  (from the input when there is none), `kablo send-key [ID] KEY...` presses
  keys (`enter`, `tab`, `escape`, `backspace`, `space`, the arrows, `home`,
  `end`, `pageup`, `pagedown`, `insert`, `delete`, `f1` to `f12`, `shift+tab`,
  `ctrl+X`, `alt+X`) as the terminal would send them in the modes the program
  has set, `kablo capture [ID] [--history ROWS] [--unwrap] [--trim] [--vt]`
  prints the screen as text or, with `--vt`, as the bytes that draw it, `kablo
  wait ID [--for TIME]` ends with the program's status (128 plus the signal's
  number after a signal) and `kablo label [ID] [TEXT]` sets or shows the
  session's label. A session is named by its id or its label (the label
  without regard to case); a name that fits more than one session does
  nothing and lists them. With no name a command means the terminal it runs
  in, and from outside every session it ends with `64`. `wait` ends with `124`
  when `--for` ran out and `255` for kablo's own trouble; the others end with
  `0`, or `1` for kablo's own trouble (`capture` ends with `141` when its output
  is closed on it). A relay replaced by `kablo upgrade`
  does not end a `wait`, and text is never typed twice. A session started by
  kablo 0.1.0 has a relay that does not know these commands: each one says so
  and names `kablo upgrade` instead of waiting for ever (README, Driving a
  session).
- **`kablo new --label TEXT` and `--wait`; `kablo list` shows labels.** A label
  is a name for a session that can be changed, at most 64 bytes. `--wait`
  prints the id, waits for the program to end, ends with its status and takes
  the session over, so it is not kept for a day. `kablo list` has a `LABEL`
  column at the end, and `kablo list --json` the key `label`.
- **`kablo end ID [--force]` ends a session's program, and `kablo here` says which
  session a program runs in.** `kablo end` hangs up on the program (the shell and
  the programs in front of it, as a closing terminal window would; `--force` kills
  them) and waits up to 5 seconds for it to end, so it works with no app attached
  and from a script or an agent. It ends with `0` once the program has ended, or
  had; with `1` when it had not ended in time, saying to try `--force`, or when
  kablo could not (no such session, a name that fits several, a relay that is too
  old, a `broken` session); `64` without a session, which is always named. As with
  `wait`, the end is kept for the clients that have not been told, so `capture`
  still reads the last output. `kablo here` prints the id of the session it runs in
  and ends with `0`, with `1` and the reason run outside every session, with `64`
  given an argument; the answer is the system's, not a variable the process could
  set, and works with any relay (README, Driving a session). A program that
  ignores `--force` too, or a `broken` session, is still ended with `kill`
  (README, Limits).

- **`kablo list` shows how long a session has been open and how long it has
  been quiet.** The new columns `AGE` and `QUIET` come before `LABEL`: the time
  since the session's program started and since it last wrote, from the process
  table and the relay's clock, without anyone attached and without reading what
  the program wrote. `-` stands for a time that is not known (a program that has
  not written since the relay began, which a replaced relay is too). `kablo
  list --json` has the keys `started`, `front_started`, `front_is_own` and
  `output_at` (README, `kablo list`).

- **`kablo list` says who is attached.** An app that attaches under a name (the
  Mac it runs on, say) is listed by that name in the `CLIENTS` column:
  `MacBook,iMac` for two, `MacBook+1` for one named and one not. A session whose
  clients have no name (`kablo connect` without `--name`, an app that gives none)
  shows how many there are, as before, and so does one served by a relay from before
  the names. `kablo list --json` has the key `clients_named`, the names in the order
  the clients' connections were made, `""` for one with none; it is left out when
  the relay does not say, except that a session nobody is attached to has `[]`
  whichever relay serves it (README, `kablo list`).

- **`kablo connect --take`: use a session from this terminal only.** The other
  clients attached to it are let go, each told which computer took it, and the
  session's terminal takes this window's size. `--name NAME` (1 to 63 bytes) is what
  this connection calls itself, to them and in `kablo list`. A `connect` whose
  session is taken says `kablo: taken by NAME; the session keeps running` (`taken by
  another client` when the one that took gave no name) and ends with `0`, as after
  Ctrl-\: the session goes on, it is just not on that terminal any more (README,
  Use).

### Fixed

- **A program kept under its version number is named by the name you started it
  with.** Claude Code, installed as a file named `2.1.296` and started as `claude`,
  was listed in `kablo list` (`PROGRAM`, `--json`'s `program`) and told to the apps
  that show the terminal as `2.1.296`; it is now `claude`. Only a program the system
  names with digits and dots alone is looked at, and only the first word of its
  command line is read for it; everything else keeps the system's name (README,
  `kablo list`).

### For apps

- **Protocol: an attached client has a name, and `status` tells whose.** The view
  of an `attach` (`docs/PROTOCOL.md`, attach) may end with a name after its flags
  byte: a length byte and that many bytes of UTF-8, at most 63. A view that ends at
  its flags, an `attach` with no view and a name of length 0 are a client with no
  name, so every `attach` from before is the bytes it was; a name too long, not all
  there or not UTF-8 makes the `attach` malformed (`BAD_REQUEST`, nothing attached,
  the connection goes on). A relay from before this release reads the view and
  ignores the name, and so does a bridge from before it (a `kablo attach` that was
  running when kablo was upgraded): it does not carry the name on. `status` (and so `list_reply` and `created`) gains key 21, one
  entry per attached client in the order their connections were made, empty for one with no name,
  as many as `clients`: a reader that does not know it skips it, and a list shorter
  than `clients` is a relay from before the names, not clients without them. The
  name is for a person to read, not an identity: two clients may have the same one,
  and one that is shown is stripped of control characters first. `kablo-proto`:
  `v1::ClientName` (at most `v1::MAX_CLIENT_NAME` bytes, made with `ClientName::new`,
  or `ClientName::clipped` for a name that may be longer), `v1::View` gains the field
  `name` (`Option<ClientName>`) and `v1::Status` gains `clients_named`
  (`Vec<Option<ClientName>>`); both are built with their constructors, so this breaks
  no one that builds them, and `Message::Attach` is unchanged. `v1::MAX_CLIENTS` (64)
  is the most connections a session serves, named now because a `status` lists no
  more clients than that. The bridge keeps the name across a resize and a new
  connection to the session, as it does the flags. `kablo list --json` gains
  `clients_named`, absent when the relay does not say.

- **Protocol: a client can take the terminal from the others.** The view of an
  `attach` gains bit 3 of its flags byte, `take`. When the server has attached the
  client, every other client attached to the session is let go: each is sent an
  `error` with the new code `TAKEN` (9) whose text is the name the client that took
  gave itself (empty if it gave none), is no longer attached, and gets no more of the
  stream; its connection stays, so a bridge goes on serving the app's other
  terminals, and the client may attach again. `input` and `resize` from it are
  answered `NOT_ATTACHED`; a connection that only waits or asks is not told. The
  terminal takes the size of the taking client's view. A server from before the bit
  ignores it and attaches the client beside the others, so an app that needs to be
  the only one reads who is attached from `status`. An error code a reader has no
  name for stays a valid `error` (`docs/PROTOCOL.md`, Compatibility): a client from
  before `TAKEN` reads it as an error about a session it is attached to, which
  `kablo connect` takes as the end of its attachment; a bridge from before it passes
  the error on but keeps the attach it remembers for a relay that starts anew, so it
  attaches the client again if its relay is replaced meanwhile (README, Limits).
  `kablo-proto`: `v1::View` gains the field `take` (`bool`) and `v1::ErrorCode` the
  constant `TAKEN`. The bridge sends a view with `take` once: it forgets the bit
  when the `attached` comes, so a connection it makes again (the relay was replaced)
  attaches as the client it is and does not let go of whoever attached since, and
  forgets the attach for a session it was told `TAKEN` about.

- **Protocol: `program` is the name a versioned release was started by.** The `program`
  of `status` (key 3, and so `list_reply` and `created`) and of `state` (key 1) is
  the name the system gives the foreground process, as before, unless that name is
  only digits and dots (a release kept as the file `2.1.296` and started through a
  link or under a name of its own): then it is the last part of the process's first
  argument (`claude`). The key, its type and its meaning, the name of the program, are
  unchanged; what changes is the value for such a program, which was the version
  number. That first word is the only part of a command line read for a program that
  is not an ssh or mosh session, the environment is still never read, and the name
  is kept as the system's when the word is empty, longer than 255 bytes or more than
  one word (a space or a control character in it). A client that shows the name to a
  person removes control characters first, as before. `docs/PROTOCOL.md` (status,
  state) and the docs of `v1::Status::program` and `v1::TerminalState` say so; the
  protocol version and the Rust API are unchanged.

- **Protocol: an ended terminal can be kept for a program that comes after the
  app.** The view of an `attach` (`docs/PROTOCOL.md`, attach) gains bit 2 of its
  flags byte, `keep after the end`. An app that sets it and is attached when the
  program ends, or attaches after, is told `ended` as before, and from that moment
  the session is kept for ten minutes more whoever else is told, whether or not the
  app stays attached: `status`, `list`, `read` and `wait` answer as they do for any
  ended session, so a program on the server that was waiting for the end of a
  terminal the app opened can still read its last screen and take its status
  (`kablo capture`, `kablo wait`); a `write` is refused and an `input` is dropped.
  The ten minutes start once, when the first such client is told (or another is,
  while such a client is attached and behind), and a session whose relay dies and
  is started again meanwhile is kept as long as before: until a client that asks is
  told again, or the day. A client that does not set the bit, and `kablo connect`
  and `kablo wait`, are as they were; `kablo new --wait` on a session an app keeps
  ends up to five seconds after the program does, because the session it would
  take over stays. A relay from before this release ignores the bit and removes the
  session as it always did: an app reads `version` in `status` before it relies on
  it. `kablo-proto`: `v1::View`
  gains the field `keep_after_end` (`bool`; it is built with `View::new`, so this
  breaks no one that builds it). The bridge keeps the bit across a resize and a new
  connection to the session, as it does the others.
- **Protocol: when things happened to a session.** `status` (and so `list_reply`
  and `created`) gains the keys 17 `started` (u64: when the session's own
  program started, in seconds since the Unix epoch of the server's clock), 18
  `front_started` (u64: when the program in the foreground did), 19
  `front_is_own` (u8: 1 when the foreground is the session's own program, which
  for a session opened with a shell is the shell waiting for a command) and 20
  `output_at` (u64: when the relay last read output, in whole seconds)
  (`docs/PROTOCOL.md`, status). They are written only when known and a reader
  that does not know them skips them, so every `status` from before is the bytes
  it was; `started` is kept after the program has ended, the others are not
  there once it has. `output_at` is the relay's clock at the read, so a relay
  does not know it until it has read output itself: not for what the program
  wrote before the relay began, nor after a relay was replaced, until the
  program writes again. An app
  reads `version` before relying on them, as with the rest.
  `kablo list --json` gains `started`, `front_started` and `output_at` (a number,
  or `null`) and `front_is_own` (`true`, `false` or `null`); nothing else of it
  changes. `kablo-proto`: `v1::Status` gains `started`, `front_started`,
  `output_at` (`Option<u64>`) and `front_is_own` (`Option<bool>`); like `label`,
  it is built with its constructor, so this breaks no one that builds it.
- **Protocol: driving a session without attaching.** An app, or a program on the
  server, can type to a session, read its screen and wait for its program to end
  from a connection that has not attached, without being counted as attached and
  without the server stopping to answer the questions the program asks its
  terminal (`docs/PROTOCOL.md`, section 5, Driving a session without attaching).
  Added to version 1 and nothing else changed: on the session channel `write`
  (0x0e: text and keys, a key being a number in the kitty keyboard protocol's
  numbering with the modifiers held), `read` (0x0f: the screen as text, with
  scrollback and wrapped rows joined if asked, or as a `picture`) and `wait`
  (0x10: answered by `ended`, which is not delivery, so the session is kept for
  the clients that have not been told) from the client, `label` (0x11: set or
  clear the session's label; the bridge answers it, as it does `new`) and
  `screen_text` (0x8e: the answer to a text `read`, in pieces with `more`) from
  the server; in `new` key 7 and in `status` key 16 (`label`, at most 64 bytes of
  UTF-8, written only when there is one); error code 8 (a `write` to a program that
  has ended). `picture` can now also answer a `read`, with no `attached` before it.
  A reader that does not know the kinds skips them, so a relay from before this
  release answers none of them: an app finds out which relay it has from `status`'s
  `version`, not from silence. A `status` and a `new` without a label are the bytes
  they were.
- **`kablo attach` serves them.** The bridge carries `write`, `read` and `wait` to
  the session's relay without attaching to it, and a `ping` that names a session
  a `write` of the client's is waiting to be confirmed at goes to that relay too,
  so its `pong` comes after the `error` that the `write` has, if it has one (any
  other `ping`, named session or not, is still the bridge's own, so a pulse is
  answered while a relay is being replaced). When the relay is replaced (`kablo upgrade`) a
  `read` and a `wait` are asked again of the new one and the client notices
  nothing; a `write` never is, because it would press the keys twice: one that
  comes while the connection to the session is being made again is answered
  `unavailable` and typed nowhere, and a `ping` whose connection is lost before
  the `pong` comes (or a `write` that finds the connection gone as it is sent) is
  answered `unavailable` instead, which says the `write` may or may not have been
  typed. The bridge answers `label` by writing the session's label to a file
  in the socket directory, which the relay reads for `status` and removes when the
  session is gone (`UNKNOWN_SESSION` when there is no such session); a `new` that
  carries a label makes it, and a session made under the name of one that is gone
  does not have its label.
- **`kablo-proto`: the API for it.** `v1::Message` gains `Write(Vec<Stroke>)`,
  `Read(Capture)`, `Wait`, `Label(String)` and `ScreenText { offset, cols, rows,
  more, data }`; `v1::kind` gains `WRITE`, `READ`, `WAIT`, `LABEL` and
  `SCREEN_TEXT`; `v1::ErrorCode` gains `ENDED`. `v1::Stroke` (not exhaustive: `text`
  or `key`, made with `Stroke::text` and `Stroke::key`), `v1::Key` (not
  exhaustive: `code` and `mods`, made with `Key::new`) and `v1::Capture` (not
  exhaustive: `picture`, `history`, `unwrap`; `to_block` and `from_block`) are new,
  as is `v1::key` with the numbers of the keys, `key::function(n)` for F1 to F35 and
  `key::mods` for the modifiers' bits. `v1::Status` and `v1::NewSession` gain the
  field `label` (`Option<String>`; both are built with their constructors, so this
  breaks no one that builds them), and `v1::MAX_LABEL` (64) and
  `v1::MAX_SCREEN_TEXT` are new. A label over `MAX_LABEL` is refused when it is
  sent and when it is read, in a `label` and in a `new`; a `status` carries what the
  server says and is read whole.
- **`kablo end` and `kablo here`, and what an app can rely on of them.** `kablo end
  ID [--force]` sends the protocol's `terminate` (`force`: `--force`) to the session
  and, on the same connection, a `wait` for 5 seconds; it ends with `0` when `ended`
  came (or the session's `status` already said `ended`, in which case nothing is
  sent), `1` when it did not come in time or kablo could not (the session cannot
  take a `terminate` when its warden is gone: `unavailable`, which it reports), and
  `64` when no session is named. The `wait` is not delivery, so the session is kept
  for the clients that have not been told, and a relay from before 0.2.0 is refused
  as the other commands do. `kablo here` prints the session's id and a newline,
  asking every running session's `status` and no more, and ends with `0`; outside
  every session with `1` and the reason, and with `64` for an argument. Both end
  as `send`, `send-key`, `capture` and `label` do: `0`, `1` for kablo's own
  trouble, `64` for a command line that does not parse.
- **The commands that drive a session, and what an app can rely on of them.** A
  relay from kablo 0.2.0 on takes the messages above and a label; a session's
  `status` says which release serves it (`version`), and a program or an app that
  wants these reads that, because an older relay answers none of them. `kablo
  list --json` gains the key `label` (a string, or `null` for a session with
  none; nothing else of it changes). The exit status of `kablo wait` and `kablo
  new --wait` is the program's (its exit code, or 128 plus the signal's number),
  `124` when `--for` ran out and `255` when kablo itself could not (no such
  session, a name that fits several, a relay that is too old, the connection
  lost); `kablo send`, `send-key`, `capture` and `label` end with `0`, with `1`
  when kablo could not, and with `64` for a command line that does not parse
  (`capture` also ends quietly with `141` if whatever reads its output goes away).
  A program's own `124` or `255` cannot be told from kablo's by the status alone.
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
- **Changed, `kablo-proto`: breaking once, for code that matches `Message`.**
  `v1::Message` and `v1::stream::Message` have the variants above now, and they
  and `v1::stream::Incoming` are `#[non_exhaustive]`, so a `match` on one of
  them with no catch-all arm no longer compiles: end it with an arm for the
  messages it does not handle. That is the last time it breaks: a variant added
  to them later breaks no app, and a message of a kind a reader does not know is
  skipped as before. A `features` that has a body reads as `FeaturesWith`, so a
  server that answers only `Message::Features` does not answer it.
- **Changed, `kablo-proto`: `Features`, `Status`, `NewSession`, `TerminalState`,
  `View` and `v1::stream::Command` are `#[non_exhaustive]`.** Code outside the
  crate could write them as struct expressions in 0.1.0 (`Features { streams:
  true, ..Features::default() }`) and can no longer. Make one with its
  constructor and set the fields, which stay public: `Features::with_streams()`,
  `Features::streams_off(why)` or `Features::default()`; `Status::new(id)`;
  `NewSession::default()`; `TerminalState::default()`; `View::new(size)`;
  `Command::new(argv)`. Reading is as before, except that a pattern naming the
  fields of one ends in `..`. This is so that a field added to them later, as
  `Features::mailbox` is in this release, breaks no app that builds them. The
  wire format is not touched.
- **The terminal emulator is a fork too, as the parser is: `vendor/alacritty_terminal`.**
  Upstream 0.26.0 with one addition, `Term::hidden_primary_grid()`, which lets the screen
  behind a full screen program be read, and one fix upstream made after the release: a full
  kitty keyboard mode stack drops its own bottom entry instead of the title stack's, which
  panicked when no title was pushed (`vendor/alacritty_terminal/FORK.md`). Same name and
  version as upstream, so a dependent resolves to it through `[patch.crates-io]`; bateri takes
  it from this repository by a pinned commit, next to `vendor/vte`. Nothing of the relay's
  behavior changes.

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
