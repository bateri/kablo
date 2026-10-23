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
lets the server pick the name; a `ping` may also name the session whose `write`
it confirms, section 5, Driving a session without attaching). A server's socket for a session is
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
  entry, in order. For any other key the last entry wins. In a `write` two keys
  are one list between them: the order of the entries is the order of the strokes.
- An entry that runs past the end of the block makes the whole block malformed.

## 4. Messages

Kinds below 0x80 go client to server, the others server to client, on both
channels. A kind means what it means on its channel: the table below is the
messages of channel 0, and the messages of channel 1 follow it.

| kind | name | session | payload |
|---|---|---|---|
| 0x01 | `attach` | yes | `offset: u64`, then optionally a view: `cols: u16, rows: u16, xpixel: u16, ypixel: u16, flags: u8` (bit 0: picture, bit 1: picture if answered, bit 2: keep after the end, bit 3: take), then optionally the client's name: `length: u8` and that many bytes of UTF-8, at most 63 |
| 0x02 | `detach` | yes | (empty) |
| 0x03 | `resize` | yes | `cols: u16, rows: u16, xpixel: u16, ypixel: u16` |
| 0x04 | `terminate` | yes | `flags: u8` (bit 0: force) |
| 0x05 | `upgrade` | yes | the relay binary's absolute path, 1 to 4096 bytes, no NUL |
| 0x06 | `status` | yes | (empty) |
| 0x07 | `input` | yes | the bytes |
| 0x08 | `list` | no | (empty) |
| 0x09 | `new` | optional | TLV block, below |
| 0x0a | `ping` | optional | any bytes |
| 0x0b | `features` | no | (empty), or a TLV block, below |
| 0x0c | `ask` | optional | (empty) |
| 0x0d | `decline` | no | `ticket: u32`, then UTF-8 text |
| 0x0e | `write` | yes | TLV block, below |
| 0x0f | `read` | yes | TLV block, below (empty: the rows on screen) |
| 0x10 | `wait` | yes | (empty) |
| 0x11 | `label` | yes | UTF-8 text of 0 to 64 bytes (empty clears) |
| 0x12 | `forget` | yes | (empty) |
| 0x81 | `attached` | yes | `offset: u64, flags: u8` (bit 0: truncated), then optionally `answered: u64` |
| 0x82 | `output` | yes | `offset: u64`, then the bytes |
| 0x83 | `ended` | yes | `kind: u8` (0 exit code, 1 signal), `value: i32` |
| 0x84 | `error` | optional | `code: u16`, then UTF-8 text |
| 0x85 | `pong` | no | the bytes of the `ping` |
| 0x86 | `status_reply` | yes | status TLV block, below |
| 0x87 | `list_reply` | no | TLV block of `session` entries, below |
| 0x88 | `created` | yes | status TLV block |
| 0x89 | `picture` | yes | `offset: u64, cols: u16, rows: u16, flags: u8` (bit 0: more), then the bytes; follows an `attached`, or answers a `read` |
| 0x8a | `features_reply` | no | TLV block, below |
| 0x8b | `state` | yes | TLV block, below |
| 0x8c | `ask_reply` | optional | TLV block, below |
| 0x8d | `ticket` | optional | `ticket: u32` |
| 0x8e | `screen_text` | yes | `offset: u64, cols: u16, rows: u16, flags: u8` (bit 0: more), then the bytes |

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
| 7 | text | `label`: the session's label, at most 64 bytes (below); empty is none |

The command is an argument vector, **never a shell string**, and a server never
builds a command line out of it. No argument, name, value or directory may
contain a NUL; a name is not empty and has no `=`; a directory is not empty.
A `new` that breaks this, or has a `label` over 64 bytes or that is not UTF-8, is malformed. A `journal` or a `history` outside its range is
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
| 3 | bytes | `program`: the name of the program in the terminal's foreground (see `state`) |
| 4 | bytes | `directory`: the foreground program's working directory |
| 5 | 8 bytes | `size`: columns, rows, xpixel, ypixel |
| 6 | u32 | `clients`: how many are attached |
| 7 | u8 | `state`: 0 running, 1 ended, 2 broken (the relay keeps failing; the shell is alive) |
| 8 | u64 | `offset`: the total bytes the program has written |
| 9 | u64 | `retained`: the oldest offset a client is given without a picture: the oldest the journal holds, or the offset the past was last forgotten at (`forget`) when that is later |
| 10 | 5 bytes | `exit`: as in `ended`, present once the program has ended |
| 11 | text | `version`: the version of the relay serving the session |
| 12 | text | `warden_version`: the version of the warden that keeps the session; absent from a warden that did not say |
| 13 | u8 | `secret`: as in `state`; written only when set |
| 14 | bytes | `remote`: as in `state` |
| 15 | u16 | `remote_port`: as in `state` |
| 16 | text | `label`: the session's label, written only when it has one (below) |
| 17 | u64 | `started`: when the session's own program (the shell, or the program `new` was given) started, in seconds since the Unix epoch of the server's clock; kept after the program has ended |
| 18 | u64 | `front_started`: when the program in the terminal's foreground started, on the same clock; only while the session runs |
| 19 | u8 | `front_is_own`: 1 when the program in the foreground is the session's own (for a session opened with a shell, the shell waiting for a command), 0 when another is; only while the session runs |
| 20 | u64 | `output_at`: when the relay last read output from the terminal, in whole seconds on the same clock; absent until it has read some |
| 21 | bytes | `client`: the name of one attached client, empty for one that gave none; repeated, one entry per client counted in `clients`, in the order their connections were made; at most 64 entries of at most 63 bytes of UTF-8 each |

Keys 17 to 20 are written only when known. They come from the process table
and from the relay's own clock, never from what the program wrote, so a
status answers them for a session nobody is attached to; a relay does not know
`output_at` until it has read output itself (what the program wrote before the
relay began, and what an earlier relay read, are not counted), and a session
that ended before the relay started has no `started`. A client compares
them with its own clock only after taking the difference of the server's clock
from its own (the times are seconds on the server).

Key 21 is written only when the server says who is attached, and then once per
client counted in key 6 (taken in the same moment as the count), so the list
and the count agree. A list shorter than the count is a server from before the
names, which counted its clients and named none: a reader must not take it for
clients that have no name, which a server that names its clients says with an
empty entry. A name is text a client chose and can hold control characters; a
client that shows one to a person removes them first. Two clients may have the
same name, and no one is told apart by it: it is for a person to read.

Only `id` is required; a reader treats an absent key as zero (or nothing).
A status takes in what the session's connections had sent by the time the
question came: a client that left, or sent `detach`, before somebody asked is
not counted in `clients`, whichever connection either of them was on.
`list_reply` is a block of entries with key 1 (`session`), each holding one
status block. Keys 3, 4 and 13 to 15 are the terminal's state as `state` says it
(below), looked at when the question came.

### state

The TLV block of `state`: what the session's terminal is doing that the bytes of
its output do not say, as only the server, which holds the terminal, can tell.
Every key is optional and an absent one reads as nothing.

| key | value | meaning |
|---|---|---|
| 1 | bytes | `program`: the name of the program in the terminal's foreground: the name the system gives the process, unless that is only digits and dots (a release kept as a file named after its version, `2.1.296`, and started through a link or under a name of its own, `claude`), and then the last part of the process's first argument; a client that shows it to a person removes control characters first |
| 2 | bytes | `directory`: the foreground program's working directory, as in `status` |
| 3 | u8 | `secret`: bit 0 is set when the terminal does not show what is typed and takes it a line at a time, which is what `sudo`, `read -s` and ssh set to ask for a password (a program that wants single keys, an editor, turns the display of what is typed off but not the line mode, and is not a secret); written only when set |
| 4 | bytes | `remote`: the target of the interactive ssh or mosh session in the foreground, as the user typed it (`user@host`, `host`, a name from ssh's configuration); an `ssh://user@host:2222` target is told as `user@host`, without its scheme and port (the port is `remote_port`); a client that shows it to a person removes control characters first |
| 5 | u16 | `remote_port`: the port the user asked ssh for (`-p`, `-o Port=`, an `ssh://` target); absent when they asked for none, which does not say what port ssh will use, and for mosh, whose `-p` is a UDP port |

A `state` is **the whole of it**: a key that is absent is nothing now, not "as
before", so a client keeps the last one it was given and replaces it. The server
sends one to a client right after `attached`, and then to every attached client
whenever any of it differs from what it last sent. A change is told a moment
after it happens rather than at the instant: the server looks at the terminal
when the program's output has arrived and shortly after the last of it, and
shortly after a client's input (a program that starts, says nothing and keeps
running is seen in that moment), and never on a timer while the session is idle.
A session nobody is attached to is not looked at; `status` looks when asked.
When the program ends the server sends a `state` with nothing in it (unless the
clients were told that already) before `ended`: a password that was being asked
for, or an ssh session, is over with it.

The server tells a client what it needs to show the program and to know when a
password is being asked for, and no more. `remote` is a target and nothing of
the rest of the command line (options, the command run on the other side, what
a wrapper was given), and the environment is never read. The one other word of
a command line that is read is the first, and only for a process the system
names with a version number, to give `program` the name the person started it
by; it is left as the system's name when that word is longer than a file name or
is more than one word. A command that is not an interactive ssh or mosh session,
or whose target cannot be told, has no `remote`. An ssh that runs a command
(`ssh host uptime`) is not an interactive session.

### features

A `features` may carry a TLV block saying what its sender can take. One without
a block says nothing, and so does one whose block cannot be read: a reader that
knows the block and finds it malformed answers as if there were none. Every key
is optional and an absent one reads as zero.

| key | value | meaning |
|---|---|---|
| 1 | u8 | `tickets`: bit 0 is set when the sender is an app that takes tickets (section 5, Mail) |

The TLV block of `features_reply` is what the server can do beyond terminal
sessions. Every key is optional and an absent one reads as zero (or nothing), so
a reply with no `streams` says there are none.

| key | value | meaning |
|---|---|---|
| 1 | u8 | `streams`: bit 0 is set when the server takes streams (channel 1) |
| 2 | text | `reason`: when `streams` is clear and the server knows streams, why they are off; text for a person, who sees it with control characters removed |
| 3 | u32 | `max_streams`: how many streams a connection may have open at once |
| 4 | u32 | `max_window`: the largest window the server takes for a stream; a client gives none larger. What the server gives is its own choice, within the limit in section 6 |
| 5 | u32 | `max_data`: the most bytes the server takes in one stream `data` |
| 6 | u8 | `commands`: bit 0 is set when the server runs programs for the client (an `open` with an `argv`, below); written only when set |
| 7 | u8 | `mailbox`: bit 0 is set when the bridge keeps a mailbox and gives its client the tickets of the mail that comes to it (section 5, Mail); written only when set |

A limit that is absent or zero is the one in section 6. `commands` is
independent of `streams`: a server whose administrator has turned streams off
(section 5) still runs commands, and says so.

### ask, ask_reply, ticket and decline

The messages of mail (section 5, Mail): a program on the server that wants to
hand bytes to the app that is connected to the server asks the bridge where it
stands, and the bridge gives the app a ticket for the bytes.

`ask` has no payload (a reader ignores what is there). Its session id, when it
has one, is the terminal the question is about; without one it is about the
terminal the asker runs in, which the bridge finds out itself. The answer is
`ask_reply`: its session id is the terminal the answer is about, or empty when
there is none, and its payload is a TLV block. Every key is optional and an absent
one reads as zero.

| key | value | meaning |
|---|---|---|
| 1 | u8 | `attached`: bit 0 is set when the bridge's client is attached to that terminal |
| 2 | u64 | `idle`: the milliseconds since the client last used that terminal, when `attached`; otherwise since it last used any terminal of this server. Using a terminal is attaching to it or sending it `input`; a client that has just connected counts as having used one just now |
| 3 | u8 | `tickets`: bit 0 is set when the client takes tickets (`tickets` in its `features`) |

The terminal of an answer to an `ask` that names none is a running session and is
never something the asker says about itself: the bridge finds it from the process
at the other end of the connection, the session whose terminal that process has or
whose shell it descends from. An `ask` that names a terminal gets an answer about
that terminal and no more; it does not change where the asker's bytes are said to
come from.

`ticket` is the bridge telling its client that bytes are waiting for it. It
carries the number of the ticket, which the bridge picks counting up from 1 on
the connection and never uses twice, and its session id is the terminal the bytes
come from (found as above), empty when they come from none. The client answers
with an `open` that carries the number (Stream messages, below) or with
`decline`, which names the ticket it refuses and gives text in the client's own
words, for a person, who sees it with control characters removed.

### write, read, wait, label and screen_text

The messages for driving a session from a connection that has not attached
(section 5, Driving a session without attaching).

**`write`** is a TLV block that is a list of strokes, an entry for each, in the
order they are made:

| key | value | meaning |
|---|---|---|
| 1 | bytes | `text`: typed as it is. A terminal takes it as UTF-8 text; a reader does not look inside it, and nothing in it is a key's name |
| 2 | 5 bytes | `key`: `code: u32, modifiers: u8`, a key pressed. A reader reads the five and ignores bytes after them |

A `key` is a number, not a name: the names (`enter`, `ctrl+c`) are for people and
belong to the command that takes them. The numbers are those of the kitty keyboard
protocol. A key that types a character is that character's Unicode code point as
the key makes it with no modifier held (99 for the key that types `c`, which with
the shift bit is `C`). A key that types none has the one number kitty gives it:

| key | number |
|---|---|
| Tab, Enter, Escape, Backspace | 9, 13, 27, 127 |
| Insert, Delete | 57348, 57349 |
| Left, Right, Up, Down | 57350, 57351, 57352, 57353 |
| Page up, Page down, Home, End | 57354, 57355, 57356, 57357 |
| F1 to F35 | 57364 to 57398 (F1 is 57364, F13 is 57376) |

The modifiers are bits, as kitty numbers them: 1 shift, 2 alt, 4 control, 8 super,
16 hyper, 32 meta (64 and 128, the lock keys, are carried and not looked at).
Every other control character is no key. A server turns each key into the bytes
the terminal would send for it in the modes it is in when the `write` is served
(arrow keys in the normal or the application form, the kitty keyboard flags the
program has set), and the numbers it can press are its own: one it cannot is
answered with `error` (`BAD_REQUEST`) and nothing of the `write` is typed. Keys
and text in one `write` are typed in order, with nothing of any other connection's
input between them. A `write` with no entries types nothing.

**`read`** is a TLV block. Every key is optional and an absent one reads as zero,
so an empty payload asks for the text of the rows on screen:

| key | value | meaning |
|---|---|---|
| 1 | u8 | `picture`: bit 0 is set to ask for the screen as the bytes that build it (`picture` messages, as for an `attach` that asks for one) in place of text; `history` and `unwrap` then say nothing |
| 2 | u32 | `history`: how many rows of scrollback to put above the rows on screen: the most recent this many, or all there are if fewer |
| 3 | u8 | `unwrap`: bit 0 is set to join the rows the terminal wrapped into the line the program wrote |

The answer is `screen_text`, or, for a `picture` read, a `picture`. Both carry
the `offset` of the program's output the screen is of (what the program had
written when the screen was read) and the size of the terminal it was read from.
A `picture` that answers a `read` has no `attached` before it, and the stream does
not go on from its offset: nothing is streamed to a connection that has not
attached. The text is one or more `screen_text` pieces in order, all with the same
`offset`, `cols` and `rows`; every one but the last has `more` set. Put together
they are the lines, scrollback first, each ended by a line feed (so no text is no
lines, and a blank row is a line feed alone). A piece may end anywhere, in the
middle of a line or of a character: read the text when the last piece is in. The
rows of an alternate screen are the ones on it, and a trailing run of blank rows is
there like any other, which is for the reader to drop.

**`wait`** has no payload. It is answered by `ended` (section 5).

**`label`** carries the label as UTF-8 text and no more. A label is a name for a
person to find a session by: free text of at most 64 bytes (bytes, not characters;
a longer one, or one that is not UTF-8, is malformed), and an empty one clears
it. A client that shows a label to a person removes control characters first. Like
`new`, `label` is a request to the bridge, which keeps the label with the session
and answers nothing when it is done (`error` when it could not be); a session's own
socket answers it `BAD_REQUEST`. A session's label is in its `status` (key 16) and
in `list_reply`, and in the `new` that makes the session (key 7); it is the
session's for as long as the session is there, and a new session with the id of one
that is gone does not have the old one's.

**`forget`** has no payload and asks the session to forget its past: the scrollback
its screen keeps and the record of the output so far. It is for a client whose
user cleared the history (the terminal has dropped it and does not want the next
`attach` to bring it back). From the moment the server has it, a client that
attaches from an offset before that moment is told `truncated` and is given a
picture of the screen, which has the rows that were on it and no scrollback from
before (rows that scroll off after it are kept as always), and `status` says the
offset in `retained`. A picture made for a client later than that one is made the
same way. Clients that are attached are not touched: the bytes they are owed are
sent as before, they keep their own scrollback, and the one that asked is not let
go for the output that was on its way when it did. The offset is the end of the
output the server had read when it got the message, so a client that attaches from
that offset or from after it is not cut short. It is a request of a connection that
is attached, as `input` is: from one that is not, it is an `error` (`NOT_ATTACHED`)
and nothing is forgotten; nothing is said when it is done. A program that holds the
alternate screen leaves the scrollback out of the server's reach, and the forget
is carried out when it gives the screen back. A server that is replaced or
restarted after a `forget` keeps the offset in its journal and forgets at it when it
builds its screen again, from the ring if the ring still holds the bytes before it,
so the screen that was on show at that moment comes back too; the forgotten
scrollback does not. A server or bridge from before this message skips it: nothing
is forgotten and nothing says so (the bridge of `kablo attach` passes it on to the
session like `input`, and one from before it, running while kablo was upgraded, skips
it like the session behind it).

### error codes

| code | meaning |
|---|---|
| 1 | unknown session |
| 2 | a session with that id exists |
| 3 | bad request (a missing id, a malformed payload, a value out of range) |
| 4 | the connection is not attached to the session |
| 5 | the relay could not be replaced; the old one keeps serving |
| 6 | the session cannot be served right now |
| 7 | an `input` or a `write` was not written: the program is not taking what was typed before it |
| 8 | a `write` was not written: the program has ended |
| 9 | taken: another client attached with the `take` bit and this connection is no longer attached to the session; the text is the name that client gave itself, empty if it gave none |

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
| 0x87 | `stderr` | the bytes (a command's standard error) |
| 0x88 | `exited` | the exit status, 5 bytes laid out as in `ended`: a kind (0 exit code, 1 signal) and an `i32` (a command's end) |

The TLV block of `open` holds a target to connect to, a command to run, or a
ticket to answer. A block with a `ticket` entry answers that ticket, and
nothing else in it is read; a block with an `argv` entry is a command, and its
`port` and `host` are not read; any other is a target. A target with no
`port` and no `host` is what a sender of mail writes to open a conversation
(section 5, Mail): an empty block.

| key | value | meaning |
|---|---|---|
| 1 | u16 | `port` |
| 2 | bytes | `host`: a name or an address (IPv4, or IPv6 without brackets), 1 to 253 bytes, no NUL |
| 3 | bytes | `argv`: one argument of the command; repeats, in order, and the first is the program |
| 4 | bytes | `env`: one variable to add to the server's environment, `NAME=VALUE` (the name is what comes before the first `=`); repeats |
| 5 | bytes | `cwd`: the directory to start in; the user's home when absent |
| 6 | u32 | `ticket`: the number of the ticket (`ticket`, above) this `open` answers |

A reader checks the structure of a stream message and nothing else: an `open`
without a `port` or a `host` reads as port 0 and an empty host, and the server
answers it with `failed` like any other `open` it cannot use (a mailbox takes
it as a conversation, section 5, Mail); so does one with a command it will not
run (below). Senders do not produce them (port 0 with a host, an empty host with
a port, a host over 253 bytes or with a NUL, a `data` or `stderr` over 32 KiB, a
command that is not valid); the one `open` with neither a port nor a host is
the sender of mail's.

A command is valid when it has a program that is not empty, no NUL appears in
any argument, variable or the directory, every variable has a name, the
directory is not empty, and the limits in section 6 hold. It is an argument
vector and never a shell string: the server starts the program as given and
builds no command line from it.

### stream failure codes

The `code` of `failed`.

| code | meaning |
|---|---|
| 1 | the target refused the connection |
| 2 | the target cannot be reached, or connecting failed in a way no other code names; on a mailbox, the bridge has no client that takes tickets |
| 3 | the host name could not be resolved |
| 4 | the target did not answer in time |
| 5 | too many streams on the connection, or a window over the largest; on a mailbox, the same of the client's answer to a ticket, or the bridge's tickets are used up |
| 6 | the server has streams turned off |
| 7 | the `open` makes no sense (port 0, an empty or too long host; for a command, one that is not valid) |
| 8 | the command could not be started; the text says why, as the system does; on a mailbox, the bridge could not look for the terminal the sender runs in |
| 9 | the app declined the ticket; the text is the app's own words, as sent in `decline` |

A code a reader has no name for is still a failure. The text is short English
that reads after a colon ("connection refused"); a client that shows it to a
person removes control characters first.

## 5. How a conversation goes

**Attach.** `attach(offset)` asks for the session's output starting at the
byte `offset` (offsets count bytes the program has written since the session
began; `u64::MAX` means "from now on"). The server answers `attached` with the
offset the stream actually starts at and how far it has answered the program's
questions (`answered`, below), then a `state` (section 4), then, if the offset
asked for is gone or the client asked for one, a picture of the screen (below),
then sends
`output` messages whose offsets are contiguous: each one's offset is the
previous one's plus its length. The
`output` data is the program's bytes as written, and an `output` never carries
more than 1 MiB minus 8 bytes. Several clients may attach to one session: all of
them receive the output, input from any of them reaches the terminal, and the
size is the last `resize`, unless one of them takes the terminal (below). `input` and `resize` from a connection that is not
attached are answered with `error` (`NOT_ATTACHED`) and do nothing; `detach`
ends the stream and keeps the connection. An `attach` offset past the end of
the stream is `BAD_REQUEST`.

Bit 2 of the view's flags, keep after the end, is for a client whose output is
shown to a program or a person that may come back for the screen after the client
has seen the program end (a terminal app, whose pane an agent on the server reads
a moment after the app shows that the program ended). It changes nothing about
the attach itself and is described under Ending. A server from before the bit
ignores it.

**Taking the terminal.** Bit 3 of the view's flags, take, makes the `attach` an
act and not only a way to look: when the server has attached the client, every
other client attached to the session is let go, each with an `error`
(`TAKEN`, code 9) whose text is the name the client that took gave itself (empty
if it gave none). The terminal takes the size of the taking client's view as it
does for any `attach` with one. A client that is let go is not attached any more
and gets no more of the stream (what was queued for it before comes first, and a
picture half sent is not finished); its connection stays open, and it may ask
what a connection that is not attached may (`status`, `read`, `wait`), attach
again, and go on using the session's other terminals if it is a bridge. Its
`input` and `resize` are answered `NOT_ATTACHED` like any other connection's.
A connection that is not attached to the session (one that only waits or asks) is
not a client of the terminal and is not told. The client that took is not told
anything, and an `attach` the server refuses (an offset past the end, a size that
is too big) takes nothing. Taking is not a state: a client that attaches again on
the same connection without the bit, or another client without it, attaches
beside the others as before, and a client that reconnects after its connection
broke attaches again as the client it is, not as one that takes again (a take
that was not answered before the connection broke is asked again, because it
did not happen). A server from before the bit ignores it and attaches the client
beside the others, and a client that needs to know the terminal is its own reads
`status` (key 21) for who is attached, as it also does after a relay was replaced
or restarted: the take is done by the relay that hears it, and a client that
reconnects across such a moment may be attached beside the one that took, without
being told.

**The client's name.** A view may end with a name: a length byte and that many
bytes of UTF-8, at most 63, which the client calls itself ("MacBook"). The
server shows it to whoever asks who is attached (`status`, key 21) and does
nothing else with it: it is not an identity, and two clients can have the same
one. An `attach` with no view, a view that ends at its flags, and a name of
length 0 are all a client with no name. The name belongs to the attach: a client
that attaches again on the same connection is named by the new view, and one
that detaches is no longer listed. A name longer than 63 bytes, one whose bytes
are not all there and one that is not UTF-8 make the `attach` malformed: the
server answers `error` (`BAD_REQUEST`), attaches nothing, and the connection
goes on. Bytes after the name are ignored. A server from before the name reads
the view and ignores the rest, so a client may always send one; it does not tell
who is attached.

The `attach` of a client that knows its terminal carries a **view**: the
terminal's size, whether it wants a picture whatever the offset (bit 0), and
whether it needs a picture rather than bytes that hold a question the server has
already answered (bit 1, "Questions answered twice" below). The
session's terminal takes that size before the answer, as if the client had sent
a `resize`, so the picture is made for it; a size with a zero column or row
count says nothing. A client without a view gets the picture for the size the
terminal has. A relay that predates views reads the offset and ignores the
rest, and answers as it always did (no picture).

**Coming back.** A client that lost its connection attaches again with the
offset after the last byte it received and gets exactly the bytes it missed, no
more and no fewer (a client that sets bit 1 of its view may get a picture
instead, "Questions answered twice" below). Output produced while nobody was
attached is kept in a bounded journal.

**Truncation and the picture.** The server keeps a screen of its own, built
from every byte the program writes with the parser bateri uses. When the
journal no longer holds the offset asked for, the server answers `attached`
with `truncated` set and the offset the journal has reached, and follows it
with a **picture**: the bytes that, written to a freshly reset terminal of the
size the picture says, build the screen the program has now (both screens and
which is active, the scrollback it was made to keep, every cell's text, colours
and attributes, links, cursors, scrolling region, tab stops, character sets,
modes, the title) and end with whatever escape sequence the stream stopped in.
The modes include SGR-pixel mouse (`?1016`) and in-band size reports (`?2048`),
which the terminal library has no bit for and the server follows in the stream
itself: the picture turns on the ones that are on (`CSI ? 1016 h`, `CSI ? 2048 h`,
after the rest of the screen and before the open sequence), a full reset (`ESC c`)
turns them off, and a picture of a session that never turned either on is the
bytes it was before. The server does not report the size when `?2048` is turned on
in a picture: that is for the terminal that gets it.
`output` goes on from the picture's `offset`, so the client has the screen and
then the program's own bytes, with nothing missing and nothing twice. A client
can ask for a picture whatever the offset (`attach` with the picture flag), as
a device that has never seen the session does. `truncated` is set when the server
decided on the picture, not when the client asked for it with that flag: because
the bytes left the journal, or because of bit 1 of the view's flags. It says the
bytes asked for are not coming and a picture is.

A picture is one or more `picture` messages, in order, all with the same
`offset`, `cols` and `rows`; every one but the last has `more` set. (A `read` can
ask for one too, with no `attached`; the section on driving a session says how.) A client
resets its terminal before the first piece and writes the bytes as they are.
An `attached` that arrives before the last piece starts everything over: the
connection the picture was coming over was replaced, and the picture is
discarded. A reader that does not know `picture` skips it: it is told `truncated`
and gets the program's bytes from the picture's offset on, none of what the
journal still held and no screen.

What a picture is not: kitty graphics images (the screen is text), the selection,
any other mode its terminal library keeps no bit for. The screen is
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
Colour, clipboard and pixel-size questions are never answered by the server;
the client's terminal answers those when it sees them, as it always does. The
kitty keyboard question is answered as a terminal that has the protocol would,
which is what the client's own terminal does when it is attached.

**Questions answered twice.** A client that is shown output again after a
question in it was answered (written while nobody was attached, or replayed
after a dropped connection) would answer it a second time, and the second
answer would be typed into the program. `attached` says how far the questions
were answered, in `answered`: every question in the output before that offset (a
question is where its last byte is) was answered by the server, or was read
while a client was attached and left to that client's terminal. The program is
owed no answer to those, and a client does not give one. A missing `answered` is
0, which says nothing was answered and is what a server from before the field
says. There are two ways for a client not to answer twice:

- A client with a terminal emulator of its own, whose answers it can hold back
  before they go to the server, discards the answers to questions in the output
  before `answered` and sends the rest as it always did. This is the duty of
  such a client; the server cannot do it for it.
- A client that writes the bytes it is given to a terminal that answers by
  itself, and cannot be told to keep some answers back, sets bit 1 of its view's
  flags. When the output from the offset it asks for holds a question that was
  answered (by the server or by a client's terminal), the server sends a picture
  of the screen instead of those bytes, with `truncated` set (as for output that
  has left the journal), and the stream goes on from the picture's offset;
  output that holds no such question is sent as bytes, and a short drop of the
  connection costs the picture only when a question was answered meanwhile. The
  server judges the output it read a stretch at a time, so a picture can take
  the place of some output that held no question beside one that did. A server
  from before the bit ignores it, and the client's terminal answers twice as it
  did.

The `answered` a client was told stays what it is for as long as it is attached,
also across a relay that is replaced (the bridge does not repeat the swallowed
`attached`). A program that asks in the moment between two relays has the
question answered by the relay that comes, and a client with a terminal of its
own, which learns nothing of it, answers it a second time.

A client that was attached when a question was read, and did not get to
answer it, leaves that question unanswered for good: it is below `answered`,
and nobody answers it later. That is a connection lost before the bytes with
the question reached the client's terminal, whether or not the server noticed
at once (a relay replaced meanwhile is the same), and for a client that sets
bit 1 it is also a picture sent in place of such bytes. A program that asked a
plain terminal in the moment its connection broke would have gone without its
answer too.

**Ending.** When the program exits, the server sends the rest of its output,
then `ended` with how it exited. The session is kept until a client has been
told, and removed after a day at most.

A client that set bit 2 of its view's flags (Attach) has the session kept for
ten minutes more: counted from the moment the first such client has been sent
`ended` (a second one, or the same one attaching again, does not start it
again), the session is not removed by the telling of any client, and it is
removed when the ten minutes are over, whether or not any client is attached
then. Meanwhile it is what it is for any ended session: `status`, `list`,
`read` (the screen as the program left it) and `wait` (answered at once) work, a
`write` is refused with `ENDED` (an `input` is dropped) and a new `attach` is
told `ended` again. The
time is the server's own and is not announced; a relay that is started again
meanwhile (after it died; an ended session is not replaced by `upgrade`) forgets
it, and the client that attaches again to the new one starts it afresh, so a
session is kept a day at most all the same. A client of this kind that is
attached but has not yet been sent `ended` (its connection is behind) when
another client is told starts the wait at that moment, so it is not left with
nothing and does not hold the session for longer than the wait. A server from
before the bit removes the session as soon as any client was told.

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

**Replacing the warden.** The same request renews the warden of the session
too, in the same stroke and without a message of its own: once the new relay
serves and the session is at rest, the warden replaces its program with that
binary (`execve`; same process, shell and descriptors) if the new relay said
it can take over from it. Nothing of this reaches a client except `status`:
`warden_version` names the release of the warden that keeps the session, and is
the release of the new relay a moment after the upgrade when the warden
followed. A warden that cannot follow (the new binary does not read what the
old one hands over, the program ended first, the file could not be run again)
stays as it was, the relay is replaced all the same, and `warden_version`
keeps the old release; a warden from before renewals existed does not report
one at all.

On failure the server answers `error` with `UPGRADE_FAILED` and a reason, the
relay that was asked keeps serving and the connection stays open: the path is
not one to run, the program has ended, or the new relay stopped before it took
over. A new relay takes no clients and reads no output until the old one has
let go, so exactly the bytes after the last one the old relay wrote are the
new relay's, with none missing and none twice.

**Pulse.** The client decides when to check that the other end is alive: it
sends `ping` and the server answers `pong` with the same bytes. The server
never speaks unprompted except to stream `output`, to say `ended` once and to say
`state` after a change of the terminal's state (which output or input brought
about), so an idle connection costs nothing.

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
`new` or a `label` is `BAD_REQUEST`. A `session` field left empty on a message that takes
one means the session of the socket.

**Through the bridge.** A client on another machine does not connect to a
session's socket: it runs `ssh host kablo attach`, and the standard input and
output of that command carry this protocol for every session on the server at
once, each message naming its session in the frame header. The bridge
(`kablo attach`) forwards a request for a session to that session's socket and
what comes back to the client, and answers four things itself because they
are about the connection: `ping` (a `pong` with the same bytes; but see Typing in
section 5 for one that names a session a `write` is waiting to be confirmed at), `list` (it asks
every session for its status and replies with the sessions that answered),
`new` (it starts the session and replies `created` with its status), `label` (it
sets or clears the label of the session) and `features` (a `features_reply`, at
once). `write`, `read` and `wait` it forwards to the session.

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
  socket, the client by handing them to its own) sends `credit`, which adds to
  the other end's window. How large a window an end gives is its own choice
  and may change while the stream lasts: credit for more than it took makes
  the window larger, credit held back makes it smaller (the other end, out of
  window, waits). Every end takes a window of up to 16 MiB, so an end never
  has more than that left to send (a client gives no more than the server's
  `max_window` when it says less); a `data` is at most 32 KiB. An end that
  breaks one of these (it sends past its window, or more than 32 KiB in a
  `data`, or sends credit that takes the other's window past 16 MiB) loses the
  stream: the one that sees it answers with `close`. What an end keeps for a
  stream is therefore at most the window it gave, and the bridge never has to
  stop reading its client to make room for one stream's bytes. A window has to
  hold what is on its way while the credit for it comes back, or a transfer
  waits a round trip for each window; one larger than that only fills the
  queues on the way, which everything else on the connection then waits
  behind. kablo gives a stream twice what the link carries in a round trip,
  measured from its own credit (the time it takes to come back as bytes, and
  the rate they come at), at least 64 KiB and at most 2 MiB (ssh's own window
  for a channel), within an equal part of 32 MiB per connection. Everything else the bridge says (session output,
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

**Commands.** The same channel runs a program on the server for the client, with
no terminal: the equivalent of `ssh host command` for the one connection that
is already there. The stream is the program's standard input, output and error;
it lasts as long as the program does and no longer than the connection.

- *Asking.* A server that runs commands sets `commands` in its `features_reply`
  (key 6), whether it takes streams or not. A client sends an `open` with an
  `argv` only to a server that has said so: one from before commands reads it
  as a target with no port and no host and answers `failed` (7), and one that
  never heard of streams says nothing.
- *Opening.* As for any stream: the client numbers it, gives a window, and the
  server answers `opened` with its own window, or `failed`. A command that is
  not valid is `failed` (7) and the program is not started; one that does not
  start (no such program, a directory that is not there, no permission, the
  system's refusal) is `failed` (8) with the system's words as the text (for a
  directory that cannot be entered, `cannot enter DIRECTORY: ` comes before
  them, since the system's words alone would name the program). The
  program starts in its own process group, with no controlling terminal, in the
  server's environment plus the `env` entries and in `cwd` or the user's home
  (a `cwd` that is not absolute counts from there; a home that is not there is
  not a reason to fail: the program starts where the bridge is).
  The client does not send `data` before `opened`.
- *The program's streams.* `data` from the client is written to the program's
  standard input, under the window the server gave; `eof` from the client
  closes it. `data` from the server is the program's standard output and
  `stderr` its standard error, kept apart; they are two pipes, so the order
  between them is the order the server happened to read them in. Both are paid
  for out of the one window the client gave: `credit` from the client adds to
  it whichever the bytes were.
- *The end.* When the program's standard output and standard error have both
  ended the server sends `eof`, then `exited` with how the program ended (a code
  or the signal that killed it, as the server's system numbers them), then
  `close`. Standard output and error end when the program and everything it
  started have closed them: a program that leaves a process holding them keeps
  the stream open until that process lets go, or the client sends `close`. A
  `data` or `stderr` after the server's `eof`, or an `exited` before it, is not
  valid.
- *Ending it early.* `close` from the client ends the stream and the program
  with it: the server signals the program's process group to end and, if it is
  still there after a short wait, to be killed. The same happens to every
  command when the connection to the bridge ends. A command lives as long as
  the connection and not longer; work that has to outlive it is a session.
- *The administrator.* The switch that turns streams off (section 5, Streams)
  does not touch commands: a person who can log in can run a program.

**Driving a session without attaching.** A program or a person on the server can
type to a session, read its screen and wait for its program to end without
attaching to it: `write`, `read` and `wait` are served to a connection that has
not sent `attach`, and a connection that sends them is not counted as attached (it
is not in `clients` of `status`, it is sent no `output`, no `state`, and it does
not stand in for a client's terminal). Nothing about the session changes because
one is there: while no client is attached the server goes on answering the
questions the program asks its terminal, whoever is driving it, and a client that
attaches meanwhile answers its own as always (section 5, Questions nobody
answers).

- *Typing.* `write` types its strokes, in order, as one piece, under the limit of
  `input` and in the same way: all of it is queued or, with `INPUT_FULL`, none of
  it. A program that has ended is answered `ENDED`, not silently. Nothing is said
  when it was taken. A sender that wants to know sends a `ping` after it, naming
  the same session, and its `pong` comes after the `error` the `write` has, if it
  has one: a `pong` with no `error` before it says the keys were queued for the
  program. (A bridge sends such a `ping` on to the session's relay only while a
  `write` of the client's to that session is not yet confirmed; any other `ping`
  is answered by the bridge itself, named session or not, so a pulse is answered
  while a relay is being replaced.) A `write` is sent **at most once**: a connection that is lost before
  the `pong` leaves the sender not knowing whether it arrived, and sending it
  again would press the keys twice, so the sender says that and does not.
- *Reading.* `read` is a question like `status`, answered after everything the
  program has written that the server has read so far, so the text is of the
  screen as of the answer and the `offset` says which. It never changes the screen
  or what the server tells a client.
- *Waiting.* `wait` is answered with `ended` once the program has ended and the
  last of what it wrote has reached the screen, so a `read` that follows sees the
  end of the output; when the program ended already, at once. It is the same
  `ended` that an attached client is sent, and hearing it is not delivery: the
  session is kept for the clients that have not been told, as in Ending, so a
  `wait` never ends a session and a terminal app showing the pane still learns the
  program ended. (A `wait` that is made after an attached client was told, and
  that client set bit 2, is still answered: the session is kept for ten minutes.) A `wait` is held for as long as the program runs, takes one of the
  session's 64 connections, and is not repeated by the server: a connection lost
  while it waits (a relay replaced by `upgrade` is one) leaves the sender to ask
  again, which the bridge does for the client that asked through it.

**Mail.** A program on the server can hand bytes to the app that is connected
to the server, and get bytes back; kablo carries them without looking at them and
says which terminal they come from. What the bytes mean is between the program
and the app. The program is the *sender*, the app is the bridge's *client*.

- *The mailbox.* A bridge may keep a mailbox: a socket of its own in the socket
  directory of the user it runs as, beside the sessions'. A sender connects to it
  as a client, with the handshake of section 1, and the bridge takes only
  connections of its own user. A bridge with a mailbox says so in `features_reply`
  (`mailbox`); an older one does not, and has none.
- *Taking tickets.* A client that wants mail says so in its `features` (`tickets`)
  and is given a ticket for each conversation that comes to it. A bridge gives
  tickets to no client that has not said so, and an old client, which never says,
  is sent nothing it does not know.
- *Asking.* A sender may connect to every mailbox on the server and send `ask`
  to each, to see where it stands before it sends anything: `ask_reply` says the
  terminal the sender runs in (found by the bridge, never by the sender), whether
  the client is attached to it, how long since the client used it and whether the
  client takes tickets. The choice among the bridges is the sender's: `kablo app`
  takes, among the clients that take tickets, the least idle of those attached
  to the terminal, and when there is none, the least idle of all. A sender that
  runs in no terminal has `attached` clear on every answer and gets the second.
- *Handing over.* The sender numbers its stream as any client does and sends an
  `open` with an empty block and a window (the bytes the client may send it). The
  bridge sends its client a `ticket`, with the terminal in the session id. The
  client opens a stream of its own, numbered as it numbers its streams, with
  `open` carrying `ticket` and a window (the bytes the sender may send it), or
  sends `decline`. When it opens, the bridge answers each end with `opened`
  carrying the window the other gave, and from then on joins the two streams: a
  `data`, `credit`, `eof` or `close` from one is the same message to the other,
  with the number of the other's stream and the kind of the bridge's end (the
  sender's `data` reaches the client as a server `data`, and the client's reaches
  the sender the same way). The windows are the two ends' own, so the bridge
  keeps no bytes of the conversation beyond what the windows let through, and
  `stderr` and `exited` are not used. A conversation counts among the streams of
  both connections.
- *When it does not get there.* The sender's `open` is answered `failed` (2) when
  the bridge has no client that takes tickets, (5) when the connection has too
  many streams, when the client's answer to the ticket has a window over the
  largest or the client has too many streams, or when the bridge's tickets are
  used up, (8) when the bridge cannot look for the terminal the sender runs in,
  and (9) with the client's text when it declines. An `open` that
  answers a ticket that is not outstanding (it was answered, or the sender gave it
  up) is `failed` (7), and so is one on a mailbox that names a target to connect to
  or a command to run: a mailbox is for neither.
- *The end.* `eof` ends one direction as in any stream, and `close` ends both. The
  client's `close` is the end of the conversation from the app's side and reaches
  the sender as `close`. A sender that gives up (a `close` for its stream, or the
  end of its connection) has the bridge forget the ticket if the client has not
  answered it, and `close` the client's stream if it has. When the client's
  connection to the bridge ends, the bridge ends with it and so does every
  connection to its mailbox, with no `close` on the streams that were open: a sender
  that sees its connection end without one knows the app went away. A sender that
  waits for the client to answer a ticket for longer than it will wait does the
  same as one that gives up: how long is its own choice, since a bridge sets no
  timers.

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
| bytes in one `screen_text` | 1 MiB minus 13 bytes |
| a `write` | one frame, 1 MiB; the bytes it comes to count against the input waiting for the program |
| a label | 64 bytes |
| connections to one session | 64 |
| input waiting for the program | 1 MiB, then an `input` is answered with `INPUT_FULL` |
| replies queued for one client | 4 MiB, then the client is dropped |
| streams open on one connection | 128, counting the ones still connecting; one more is `failed` (5) |
| streams connecting at once on one connection | 16; one more is `failed` (5) |
| window of a stream, and credit outstanding | 16 MiB; an `open` asking for more is `failed` (5) |
| bytes in one stream `data` or `stderr` | 32 KiB |
| time to connect a stream | 10 seconds, then `failed` (4) |
| host in an `open` | 253 bytes |
| arguments of a command, the program included | 1024; more is `failed` (7) |
| environment entries of a command | 256; more is `failed` (7) |
| bytes in a command's arguments, variables and directory together | 256 KiB; more is `failed` (7) |

## 7. Compatibility

- Every end speaks a range of versions, and the server selects the highest
  both speak. A build reads **its current version and the one before it**.
- Inside a version the format grows only in ways a reader already tolerates:
  new message kinds and channels (skipped: a reader without streams skips
  channel 1, one without `features` skips that kind), new TLV keys (skipped),
  fields appended to a fixed layout (ignored), new bits in a flag byte
  (ignored), new error codes, failure codes and states (carried as numbers).
- An error code a reader has no name for is still an `error` with its number and
  text, and a new code may say that something the client had was ended: `TAKEN`
  ends the client's attachment, and the server has ended it already when it sends
  the code. A client from before the code reads it as an error about a session it
  is attached to that it has no request of its own to answer, and the safe reading
  of that is that the attachment is over, which `kablo connect` does and has
  since 0.1.0 (it leaves the terminal with the text). The bridge of `kablo attach` from
  before the code passes the error on to its client but keeps the attach it
  remembers for a relay that starts anew, so for that bridge the client is listed
  again as attached when the relay is replaced or restarted (README, Limits). That
  bridge does not carry the name or the bit of a view on either.
- Anything else is a new version. The bytes of every message of every shipped
  version are kept as fixtures that a test reads and compares in both
  directions; a fixture is never edited.
