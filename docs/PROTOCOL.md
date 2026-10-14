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
1       1     channel       0 = terminal sessions; nothing else is defined
2       1     session_len   0 to 64
3       4     payload_len   0 to 1 048 576 (1 MiB)
7       n     session id    session_len bytes
7+n     m     payload       payload_len bytes
```

- A frame whose `channel` is not 0, or whose `kind` the reader does not know, is
  **skipped** by its lengths: the reader discards `session_len + payload_len`
  bytes and carries on. Senders of this version always write channel 0; the
  other values are reserved (for forwarding other kinds of streams later).
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
`<id>.sock`, which is why the alphabet has no separator.

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

Kinds below 0x80 go client to server, the others server to client.

| kind | name | session | payload |
|---|---|---|---|
| 0x01 | `attach` | yes | `offset: u64` |
| 0x02 | `detach` | yes | (empty) |
| 0x03 | `resize` | yes | `cols: u16, rows: u16, xpixel: u16, ypixel: u16` |
| 0x04 | `terminate` | yes | `flags: u8` (bit 0: force) |
| 0x05 | `upgrade` | yes | the relay binary's absolute path, 1 to 4096 bytes, no NUL |
| 0x06 | `status` | yes | (empty) |
| 0x07 | `input` | yes | the bytes |
| 0x08 | `list` | no | (empty) |
| 0x09 | `new` | optional | TLV block, below |
| 0x0a | `ping` | no | any bytes |
| 0x81 | `attached` | yes | `offset: u64, flags: u8` (bit 0: truncated) |
| 0x82 | `output` | yes | `offset: u64`, then the bytes |
| 0x83 | `ended` | yes | `kind: u8` (0 exit code, 1 signal), `value: i32` |
| 0x84 | `error` | optional | `code: u16`, then UTF-8 text |
| 0x85 | `pong` | no | the bytes of the `ping` |
| 0x86 | `status_reply` | yes | status TLV block, below |
| 0x87 | `list_reply` | no | TLV block of `session` entries, below |
| 0x88 | `created` | yes | status TLV block |

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

The command is an argument vector, **never a shell string**, and a server never
builds a command line out of it. No argument, name, value or directory may
contain a NUL; a name is not empty and has no `=`; a directory is not empty.
A `new` that breaks this is malformed. A `journal` outside its range is
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

Only `id` is required; a reader treats an absent key as zero (or nothing).
`list_reply` is a block of entries with key 1 (`session`), each holding one
status block.

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

## 5. How a conversation goes

**Attach.** `attach(offset)` asks for the session's output starting at the
byte `offset` (offsets count bytes the program has written since the session
began; `u64::MAX` means "from now on"). The server answers `attached` with the
offset the stream actually starts at, then sends `output` messages whose offsets
are contiguous: each one's offset is the previous one's plus its length. The
`output` data is the program's bytes as written, and an `output` never carries
more than 1 MiB minus 8 bytes. Several clients may attach to one session: all of
them receive the output, input from any of them reaches the terminal, and the
size is the last `resize`. `input` and `resize` from a connection that is not
attached are answered with `error` (`NOT_ATTACHED`) and do nothing; `detach`
ends the stream and keeps the connection. An `attach` offset past the end of
the stream is `BAD_REQUEST`.

**Coming back.** A client that lost its connection attaches again with the
offset after the last byte it received and gets exactly the bytes it missed, no
more and no fewer. Output produced while nobody was attached is kept in a
bounded journal.

**Truncation.** When the journal no longer holds the offset asked for, the
server answers `attached` with `truncated` set and an offset it can serve: the
start of the first line in what remains (the byte after the first newline), so
that the stream does not begin in the middle of an escape sequence. A
truncated `attach` also makes the terminal one row smaller and back, so that a
full-screen program, which gets `SIGWINCH` for each change, draws itself
again.

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
refused: it waits, unaccepted, until a seat frees. Typed input is handed to the
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
what comes back to the client, and answers three things itself because they
are about the connection: `ping` (a `pong` with the same bytes), `list` (it asks
every session for its status and replies with the sessions that answered) and
`new` (it starts the session and replies `created` with its status).

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
  moved past that byte the new `attached` carries `truncated` and is passed on.
- When the client's end of the connection closes, the bridge ends and nothing
  else does: no session is closed by it.

## 6. Limits

| what | limit |
|---|---|
| noise before the signature | 64 KiB |
| session id | 64 bytes |
| frame payload | 1 MiB |
| upgrade path | 4096 bytes |
| journal capacity (`new`) | 16 KiB to 1 GiB, 8 MiB by default |
| connections to one session | 64 |
| input waiting for the program | 1 MiB, then an `input` is answered with `INPUT_FULL` |
| replies queued for one client | 4 MiB, then the client is dropped |

## 7. Compatibility

- Every end speaks a range of versions, and the server selects the highest
  both speak. A build reads **its current version and the one before it**.
- Inside a version the format grows only in ways a reader already tolerates:
  new message kinds and channels (skipped), new TLV keys (skipped), fields
  appended to a fixed layout (ignored), new bits in a flag byte (ignored),
  new error codes and states (carried as numbers).
- Anything else is a new version. The bytes of every message of every shipped
  version are kept as fixtures that a test reads and compares in both
  directions; a fixture is never edited.
