# kablo wire protocol, version 1

This describes the bytes a kablo client and a kablo relay exchange. The
`kablo-proto` crate is a reference implementation of everything below, and its
tests read the byte-for-byte examples in `crates/kablo-proto/tests/fixtures/v1/`
(annotated hex, one file per message): when this text and a fixture disagree,
the fixture is right and this text has a bug.

The protocol runs over **any ordered byte stream**: a unix socket, the standard
input and output of `ssh host kablo attach`, a pipe. It does not care where the
bytes go, and it adds nothing to the program's own bytes: what a program writes
to its terminal travels inside `output` messages exactly as written, and what
the client sends as `input` reaches the terminal exactly as sent. Everything
that is not those bytes (framing, the handshake, replies) is outside the
terminal's stream.

All integers are big-endian.

## 1. Opening a connection

```text
client -> server   signature | min: u8 | max: u8
server -> client   signature | selected: u8 | min: u8 | max: u8
```

- `signature` is the six bytes `4b 41 42 4c 4f 00` (`KABLO` and a NUL).
- `min..=max` is the range of protocol versions the sender speaks; both are at
  least 1 and `min <= max`.
- The server selects the **highest version both ends speak** and says it in
  `selected`. When there is none it sends `selected = 0` (with its own range, so
  the client can say what is wrong) and closes the connection.
- The client speaks first.

**Bytes before the signature are ignored**, on both sides, up to 64 KiB: the far
end of an `ssh` command may print a banner, a message of the day or the output of
a shell startup file before the program starts, and none of it belongs to the
protocol. A reader scans for the signature and consumes exactly the handshake,
nothing after it. An end that finds no signature in 64 KiB gives up.

This document is version 1. There is no other version yet.

## 2. Frames

After the handshake the connection is a sequence of frames in both directions:

```text
offset  size  field
0       1     kind          what the payload is (section 4)
1       1     channel       0 = terminal sessions, 1 = streams; nothing else is defined
2       1     session_len   0 to 64
3       4     payload_len   0 to 1 048 576 (1 MiB)
7       n     session id    session_len bytes
7+n     m     payload       payload_len bytes
```

- A frame whose `channel` the reader does not know, or whose `kind` it does not
  know on that channel, is **skipped** by its lengths: the reader discards
  `session_len + payload_len` bytes and carries on. A reader that predates
  streams skips every frame of channel 1 this way. The messages about sessions
  are written on channel 0 and the messages about streams (section 4) on
  channel 1; the other values are reserved.
- `session_len` above 64 or `payload_len` above 1 MiB is a protocol violation:
  the reader rejects the header **without reading or reserving memory for the
  body** and closes the connection, since the stream can no longer be trusted.
- A stream may end between frames (a clean close). Ending inside one is an
  error.

### Session ids

A session is named by 1 to 64 bytes of `[A-Za-z0-9._-]`. The id in a frame
header says which session the message concerns; it is empty for a message about
the connection itself (`list`, `ping`, `pong`, `list_reply`, and a `new` that
lets the server pick the name). A server's socket for a session is
`<id>.sock`, which is why the alphabet has no separator. A frame on channel 1
has none: it names its stream in the payload.

## 3. Payload shapes

Two shapes are used.

**Fixed layouts**, listed field by field in section 4. A reader reads the
fields it knows from the front of the payload and **ignores any bytes after
them**, so a later version can append fields. Flag bytes: a reader looks only
at the bits it defines.

**TLV blocks**, for anything that may grow: a run of entries, each

```text
key: u16 | length: u32 | value: length bytes
```

- A reader **skips a key it does not know**.
- A key never changes meaning or type; growth is a new key. An integer value has
  exactly its defined length (a 4-byte value that is 5 bytes long is malformed);
  the 8-byte `size` and 5-byte `exit` values are fixed layouts and may gain
  bytes at their end.
- A key may repeat. For a list (`argument`, `environment`) each element is one
  entry, in order. For any other key the last entry wins.
- An entry that runs past the end of the block makes the whole block malformed.

## 4. Messages

Kinds below 0x80 go client to server, the others server to client, on both
channels. A kind means what it means on its channel: the table below is the
messages of channel 0, and the messages of channel 1 follow it.

| kind | name | session | payload |
|---|---|---|---|
| 0x01 | `attach` | yes | `offset: u64`, then optionally a view: `cols: u16, rows: u16, xpixel: u16, ypixel: u16, flags: u8` (bit 0: picture) |
| 0x02 | `detach` | yes | (empty) |
| 0x03 | `resize` | yes | `cols: u16, rows: u16, xpixel: u16, ypixel: u16` |
| 0x04 | `terminate` | yes | `flags: u8` (bit 0: force) |
| 0x05 | `upgrade` | yes | the relay binary's absolute path, 1 to 4096 bytes, no NUL |
| 0x06 | `status` | yes | (empty) |
| 0x07 | `input` | yes | the bytes |
| 0x08 | `list` | no | (empty) |
| 0x09 | `new` | optional | TLV block, below |
| 0x0a | `ping` | no | any bytes |
| 0x0b | `features` | no | (empty) |
| 0x81 | `attached` | yes | `offset: u64, flags: u8` (bit 0: truncated) |
| 0x82 | `output` | yes | `offset: u64`, then the bytes |
| 0x83 | `ended` | yes | `kind: u8` (0 exit code, 1 signal), `value: i32` |
| 0x84 | `error` | optional | `code: u16`, then UTF-8 text |
| 0x85 | `pong` | no | the bytes of the `ping` |
| 0x86 | `status_reply` | yes | status TLV block, below |
| 0x87 | `list_reply` | no | TLV block of `session` entries, below |
| 0x88 | `created` | yes | status TLV block |
| 0x89 | `picture` | yes | `offset: u64, cols: u16, rows: u16, flags: u8` (bit 0: more), then the bytes |
| 0x8a | `features_reply` | no | TLV block, below |

The first six client messages (`attach`, `detach`, `resize`, `terminate`,
`upgrade`, `status`) are the core a relay must always understand.

### new

TLV block. Every key is optional; an empty payload starts the user's login shell
at the server's default size.

| key | value | meaning |
|---|---|---|
| 1 | bytes | `argument`: one element of the command's argument vector (repeated, in order); none means the login shell |
| 2 | bytes | `environment`: `NAME=value`, split at the first `=` (repeated); added to the defaults |
| 3 | bytes | `directory`: where to start |
| 4 | 8 bytes | `size`: columns, rows, xpixel, ypixel as in `resize`; a size with a zero column or row count means the default (80 by 24) |
| 5 | u64 | `journal`: the journal's capacity in bytes, from 16 KiB to 1 GiB; the default is 8 MiB |
| 6 | u32 | `history`: how many rows of scrollback the session's screen keeps, from 0 to 100000; the default is 2000 |

The command is an argument vector, **never a shell string**, and a server never
builds a command line out of it. No argument, name, value or directory may
contain a NUL; a name is not empty and has no `=`; a directory is not empty.
A `new` that breaks this is malformed. A `journal` or a `history` outside its range is
answered with `error` (`BAD_REQUEST`) and starts nothing.

The session id in the header is the name asked for; a header with no id lets
the server choose one. The answer is `created` with the id that was used, or
`error`.

### status

The TLV block of `status_reply` and `created`, and the value of each entry in
`list_reply`.

| key | value | meaning |
|---|---|---|
| 1 | text | `id` (required) |
| 2 | u32 | `pid`: the shell's process id |
| 3 | bytes | `program`: the name of the program in the terminal's foreground |
| 4 | bytes | `directory`: the foreground program's working directory |
| 5 | 8 bytes | `size`: columns, rows, xpixel, ypixel |
| 6 | u32 | `clients`: how many are attached |
| 7 | u8 | `state`: 0 running, 1 ended, 2 broken (the relay keeps failing; the shell is alive) |
| 8 | u64 | `offset`: the total bytes the program has written |
| 9 | u64 | `retained`: the oldest offset the journal still holds |
| 10 | 5 bytes | `exit`: as in `ended`, present once the program has ended |
| 11 | text | `version`: the version of the relay serving the session |
| 12 | text | `warden_version`: the version of the warden that keeps the session; absent from a warden that did not say |

Only `id` is required; a reader treats an absent key as zero (or nothing).
A status takes in what the session's connections had sent by the time the
question came: a client that left, or sent `detach`, before somebody asked is
not counted in `clients`, whichever connection either of them was on.
`list_reply` is a block of entries with key 1 (`session`), each holding one
status block.

### features

The TLV block of `features_reply`: what the server can do beyond terminal
sessions. Every key is optional and an absent one reads as zero (or nothing), so
a reply with no `streams` says there are none.

| key | value | meaning |
|---|---|---|
| 1 | u8 | `streams`: bit 0 is set when the server takes streams (channel 1) |
| 2 | text | `reason`: when `streams` is clear and the server knows streams, why they are off; text for a person, who sees it with control characters removed |
| 3 | u32 | `max_streams`: how many streams a connection may have open at once |
| 4 | u32 | `max_window`: the largest window the server takes for a stream and offers for one |
| 5 | u32 | `max_data`: the most bytes the server takes in one stream `data` |

A limit that is absent or zero is the one in section 6.

### error codes

| code | meaning |
|---|---|
| 1 | unknown session |
| 2 | a session with that id exists |
| 3 | bad request (a missing id, a malformed payload, a value out of range) |
| 4 | the connection is not attached to the session |
| 5 | the relay could not be replaced; the old one keeps serving |
| 6 | the session cannot be served right now |
| 7 | an `input` was not written: the program is not taking what was typed before it |

A code a reader has no name for is still a valid error. The text is for logs: a
client that shows it to a person removes control characters first.

### Stream messages

The messages of channel 1. The frame's session id is empty (a reader ignores
what is there) and the payload starts with `stream: u32`, the number of the
stream. Payloads are fixed layouts, read as a prefix, except where a TLV block
is said. `data`, `credit`, `eof` and `close` go both ways and have a kind for
each.

| kind | name | payload after `stream` |
|---|---|---|
| 0x01 | `open` | `window: u32`, then a TLV block, below |
| 0x02 | `data` | the bytes |
| 0x03 | `credit` | `bytes: u32` |
| 0x04 | `eof` | (empty) |
| 0x05 | `close` | (empty) |
| 0x81 | `opened` | `window: u32` |
| 0x82 | `data` | the bytes |
| 0x83 | `credit` | `bytes: u32` |
| 0x84 | `eof` | (empty) |
| 0x85 | `close` | (empty) |
| 0x86 | `failed` | `code: u16`, then UTF-8 text |

The TLV block of `open`, the target to connect to:

| key | value | meaning |
|---|---|---|
| 1 | u16 | `port` |
| 2 | bytes | `host`: a name or an address (IPv4, or IPv6 without brackets), 1 to 253 bytes, no NUL |

A reader checks the structure of a stream message and nothing else: an `open`
without a `port` or a `host` reads as port 0 and an empty host, and the server
answers it with `failed` like any other `open` it cannot use. Senders do not
produce them (port 0, an empty host, a host over 253 bytes or with a NUL, a
`data` over 32 KiB).

### stream failure codes

The `code` of `failed`.

| code | meaning |
|---|---|
| 1 | the target refused the connection |
| 2 | the target cannot be reached, or connecting failed in a way no other code names |
| 3 | the host name could not be resolved |
| 4 | the target did not answer in time |
| 5 | too many streams on the connection, or a window over the largest |
| 6 | the server has streams turned off |
| 7 | the `open` makes no sense (port 0, an empty or too long host) |

A code a reader has no name for is still a failure. The text is short English
that reads after a colon ("connection refused"); a client that shows it to a
person removes control characters first.

## 5. How a conversation goes

**Attach.** `attach(offset)` asks for the session's output starting at the
byte `offset` (offsets count bytes the program has written since the session
began; `u64::MAX` means "from now on"). The server answers `attached` with the
offset the stream actually starts at, then, if the offset asked for is gone
or the client asked for one, a picture of the screen (below), then sends
`output` messages whose offsets are contiguous: each one's offset is the
previous one's plus its length. The
`output` data is the program's bytes as written, and an `output` never carries
more than 1 MiB minus 8 bytes. Several clients may attach to one session: all of
them receive the output, input from any of them reaches the terminal, and the
size is the last `resize`. `input` and `resize` from a connection that is not
attached are answered with `error` (`NOT_ATTACHED`) and do nothing; `detach`
ends the stream and keeps the connection. An `attach` offset past the end of
the stream is `BAD_REQUEST`.

The `attach` of a client that knows its terminal carries a **view**: the
terminal's size, and whether it wants a picture whatever the offset. The
session's terminal takes that size before the answer, as if the client had sent
a `resize`, so the picture is made for it; a size with a zero column or row
count says nothing. A client without a view gets the picture for the size the
terminal has. A relay that predates views reads the offset and ignores the
rest, and answers as it always did (no picture).

**Coming back.** A client that lost its connection attaches again with the
offset after the last byte it received and gets exactly the bytes it missed, no
more and no fewer. Output produced while nobody was attached is kept in a
bounded journal.

**Truncation and the picture.** The server keeps a screen of its own, built
from every byte the program writes with the parser bateri uses. When the
journal no longer holds the offset asked for, the server answers `attached`
with `truncated` set and the offset the journal has reached, and follows it
with a **picture**: the bytes that, written to a freshly reset terminal of the
size the picture says, build the screen the program has now (both screens and
which is active, the scrollback it was made to keep, every cell's text, colours
and attributes, links, cursors, scrolling region, tab stops, character sets,
modes, the title) and end with whatever escape sequence the stream stopped in.
`output` goes on from the picture's `offset`, so the client has the screen and
then the program's own bytes, with nothing missing and nothing twice. A client
can ask for a picture whatever the offset (`attach` with the picture flag), as
a device that has never seen the session does.

A picture is one or more `picture` messages, in order, all with the same
`offset`, `cols` and `rows`; every one but the last has `more` set. A client
resets its terminal before the first piece and writes the bytes as they are.
An `attached` that arrives before the last piece starts everything over: the
connection the picture was coming over was replaced, and the picture is
discarded. A reader that does not know `picture` skips it: it is told `truncated`
and gets the program's bytes from the picture's offset on, none of what the
journal still held and no screen.

What a picture is not: kitty graphics images (the screen is text), the selection,
a mode its terminal library keeps no bit for (SGR-pixel mouse). The screen is
the server's, kept at the size of the last `resize` or `attach` view (a terminal
that has ended keeps the size it ended at); a relay that is replaced or
restarted rebuilds it from what the journal holds, from its start if nothing
has been dropped and the journal is not very large, and from the first line
break of the last 64 MiB of it otherwise, so a screen whose beginning was
dropped (a full-screen program entered long ago) may lack state the dropped
bytes set.

A client that is slow to take a picture while the program writes faster than the
journal holds is let go when the journal passes the picture's offset, and asks
again with the picture still owed to it; the bridge does this for a client that
did not notice, and a fast enough link never has the problem.

**Questions nobody answers.** A program may ask its terminal a question (device
attributes, the cursor position, a mode, the size in characters, whether the
kitty keyboard protocol is there). The server's screen answers those that need
nothing from the terminal's owner, as the parser would for any terminal, and its
answer reaches the program **only while no client is attached**: with one
attached the client's own terminal answers, and two answers to one question
would be typed into the program. The server keeps in the journal how far the
questions were answered, so what it reads while it serves, and what the keeper
read while no server did (a program that asks at once, before the relay is up;
the moment between one relay and the next), is answered once, by whichever relay
comes to it first, and what an earlier relay answered is not answered again.
Colour, clipboard and pixel-size questions are never answered by the server. A
client that replays output it missed from the journal sees the questions that
were asked meanwhile and answers them again. The kitty keyboard question is
answered as a terminal that has the protocol would, which is what the client's
own terminal does when it is attached.

**Ending.** When the program exits, the server sends the rest of its output,
then `ended` with how it exited. The session is kept until a client has been
told, and removed after a day at most.

**Closing.** A connection that closes, for any reason, ends no session. Only
`terminate` does: a hang-up to the shell and its foreground process group, or
SIGKILL with `force`.

**Replacing the relay.** `upgrade` names the absolute path of another `kablo`
binary; the session's relay is replaced by that one without ending the shell.
The path must be a regular file that can be run, owned by the user or by the
superuser and writable by nobody else. On success the server says nothing: the
connection that asked, like every other connection to the session, is closed
(the relay it was connected to is gone) and a client connects again, as it
does after a relay dies, and attaches from the byte after the last it had.
A client that goes through the bridge (below) never sees the closing, because
the bridge connects again for it. What tells it the new relay serves is
`status`: its `version` names the release of the relay that answers (an
upgrade to a binary of the same release cannot be told apart that way).

On failure the server answers `error` with `UPGRADE_FAILED` and a reason, the
relay that was asked keeps serving and the connection stays open: the path is
not one to run, the program has ended, or the new relay stopped before it took
over. A new relay takes no clients and reads no output until the old one has
let go, so exactly the bytes after the last one the old relay wrote are the
new relay's, with none missing and none twice.

**Pulse.** The client decides when to check that the other end is alive: it
sends `ping` and the server answers `pong` with the same bytes. The server
never speaks unprompted except to stream `output` and to say `ended` once, so an
idle connection costs nothing.

**A slow reader.** A client that does not read its connection never holds up
the program. Output is sent a frame at a time and only when the connection
takes it, so the only buffer a client costs is one frame and its position in
the journal. A client is dropped when the journal has moved past the next byte
it is owed: its connection is closed, without a message, and it comes back
with the offset it had. Replies to a client's requests are queued for it and a
client that asks without ever reading is dropped the same way once 4 MiB of
them are waiting.

**Many clients.** A session serves 64 connections at once. A 65th is not
refused: it waits, unaccepted, until a seat frees, and so does one that comes
while the server has no descriptor left to give it. Typed input is handed to the
program as it takes it, and up to 1 MiB of it waits for a program that is not
reading. An `input` that would take what is waiting past that is not written,
not even its first part, and its sender gets `error` (`INPUT_FULL`, code 7);
the bytes refused are ones the program was not going to read. Every other
request is served as always, so a session whose program ignores its input can
still be asked about (`status`) and ended (`terminate`), and its output goes
on flowing. A client that wants the refused bytes to arrive sends them again
once the program reads.

**A session's socket.** Each session has a socket of its own and a connection
to it speaks about that one session: a message naming another session is
answered with `UNKNOWN_SESSION`, a `list` lists that session alone, and a
`new` is `BAD_REQUEST`. A `session` field left empty on a message that takes
one means the session of the socket.

**Through the bridge.** A client on another machine does not connect to a
session's socket: it runs `ssh host kablo attach`, and the standard input and
output of that command carry this protocol for every session on the server at
once, each message naming its session in the frame header. The bridge
(`kablo attach`) forwards a request for a session to that session's socket and
what comes back to the client, and answers four things itself because they
are about the connection: `ping` (a `pong` with the same bytes), `list` (it asks
every session for its status and replies with the sessions that answered),
`new` (it starts the session and replies `created` with its status) and
`features` (a `features_reply`, at once).

- A `list` leaves out a socket nobody listens on (a session that was killed
  leaves its file behind) and reports a socket that takes connections but never
  answers as `state` 2, *broken*: its relay is gone for good and its keeper
  alone is left. That is told after a short wait (a second and a half).
- A request for a session the server does not have is answered with
  `UNKNOWN_SESSION`; one for a session that does not answer within five seconds
  with `UNAVAILABLE`, after which the next request tries again.
- A session's relay may die or be replaced while a client is attached. The
  bridge connects again, attaches again from the byte after the last one it
  gave the client and does not tell the client: the stream goes on with
  contiguous offsets and no second `attached`. If the journal has already
  moved past that byte the new `attached` carries `truncated` and is passed
  on, with the picture that follows it; so is the `attached` of a connection
  lost in the middle of a picture, which the bridge asks to send the picture
  again.
- When the client's end of the connection closes, the bridge ends and nothing
  else does: no session is closed by it.

**Streams.** The bridge can also connect to a TCP port the server user can
reach and carry the bytes of that connection over the same standard input and
output, on channel 1: what `ssh -L` does, without asking sshd for anything, so
it works where sshd's forwarding is off. One TCP connection is one **stream**.

- *Asking.* An old bridge knows nothing of streams and says nothing about it, so
  a client asks before it relies on them: it sends `features` and, right after,
  `ping`. A bridge with streams answers `features_reply` as it reads the
  `features`, so before the `pong`. A client that reads the `pong` without
  having read a `features_reply` (or reads something other than a
  `features_reply` first, an `error` for instance) is talking to a bridge
  without streams and sends nothing on channel 1. A `features_reply` whose
  `streams` is clear says the same, with a `reason` when there is one. There is
  no timeout in this and the handshake is the same as ever.
- *Opening.* The client numbers its streams from 1 upward and never uses a
  number twice on a connection, so that a frame of a stream that is over cannot
  be taken for one of a later stream. It sends `open` with the target and a
  window (below). The bridge resolves the name and connects without holding up
  anything else on the connection, for at most 10 seconds, to any host and port
  the user it runs as can reach, as `ssh -L` does, and answers `opened` with its
  own window, or `failed` with a code and a text, after which the stream never
  existed. The client does not send `data` before `opened`, and it does not read
  the local socket it is carrying until then: what a program writes meanwhile
  waits in that socket. A bridge ignores an `open` whose number is not above
  every number it has seen on the connection.
- *Windows.* Each end gives the other a window per stream: the bytes the other
  may send before it has to wait. The client's is in `open` and is what the
  server may send; the server's is in `opened` and is what the client may send.
  An end that has taken bytes (the server by writing them to the target's
  socket, the client by handing them to its own) sends `credit` with their
  number, which adds to the other end's window. A window is at most 256 KiB,
  and an end never has more than that left to send; a `data` is at most 32 KiB.
  An end that breaks one of these (it sends past its window, or more than 32
  KiB in a `data`, or sends credit that takes the other's window past the
  largest) loses the stream: the one that sees it answers with `close`. What an end keeps for a stream is therefore at
  most one window, and the bridge never has to stop reading its client to make
  room for one stream's bytes. Everything else the bridge says (session output,
  replies) goes in the same byte stream between the pieces of stream data, which
  is why `data` comes in pieces.
- *Half and whole closes.* `eof` says that the sender sends no more `data` on
  the stream; the other direction goes on. At the target that is a shutdown of
  the writing half of the socket, and the target's own shutdown or close comes
  to the client as `eof` from the server. A stream is over when both ends have
  sent `eof`, or when either sends `close`, which ends it in both directions at
  once, with bytes still on their way dropped; the bridge sends `close` when
  the target's socket fails. A `data` after the same end's `eof` is not valid:
  the receiver ends the stream with `close`.
- *Frames of a stream that is over.* A `data`, `credit`, `eof` or `close`
  naming a stream the receiver does not have (it is over, or was never
  opened) is **ignored**, not an error: an end that sent `close` has frames of
  the stream still on their way to the other.
- *With the connection.* Streams last as long as the connection to the bridge
  does. When it ends, the bridge closes the sockets of all its streams; a client
  that connects again has none, since a TCP connection cannot be picked up where
  it was left, unlike a session.
- *The administrator.* A server whose administrator has turned streams off says
  so in `features_reply` (`streams` clear, with a `reason`) and answers an
  `open` that comes anyway with `failed` (6). The bridge asks again at each
  `open`, so a decision applies to the streams opened after it; open streams
  go on. A server that cannot tell whether its administrator has decided (the
  place where the decision is kept cannot be looked at by the bridge's user)
  counts as one that has, and says so in the `reason`.

## 6. Limits

| what | limit |
|---|---|
| noise before the signature | 64 KiB |
| session id | 64 bytes |
| frame payload | 1 MiB |
| upgrade path | 4096 bytes |
| journal capacity (`new`) | 16 KiB to 1 GiB, 8 MiB by default |
| scrollback of the screen (`new`) | 0 to 100000 rows, 2000 by default |
| terminal size (`new`, `resize`, an `attach` view) | at most 1000 columns by 1000 rows; more is `BAD_REQUEST` |
| bytes in one `picture` | 1 MiB minus 13 bytes |
| connections to one session | 64 |
| input waiting for the program | 1 MiB, then an `input` is answered with `INPUT_FULL` |
| replies queued for one client | 4 MiB, then the client is dropped |
| streams open on one connection | 128, counting the ones still connecting; one more is `failed` (5) |
| streams connecting at once on one connection | 16; one more is `failed` (5) |
| window of a stream, and credit outstanding | 256 KiB; an `open` asking for more is `failed` (5) |
| bytes in one stream `data` | 32 KiB |
| time to connect a stream | 10 seconds, then `failed` (4) |
| host in an `open` | 253 bytes |

## 7. Compatibility

- Every end speaks a range of versions, and the server selects the highest
  both speak. A build reads **its current version and the one before it**.
- Inside a version the format grows only in ways a reader already tolerates:
  new message kinds and channels (skipped: a reader without streams skips
  channel 1, one without `features` skips that kind), new TLV keys (skipped),
  fields appended to a fixed layout (ignored), new bits in a flag byte
  (ignored), new error codes, failure codes and states (carried as numbers).
- Anything else is a new version. The bytes of every message of every shipped
  version are kept as fixtures that a test reads and compares in both
  directions; a fixture is never edited.
