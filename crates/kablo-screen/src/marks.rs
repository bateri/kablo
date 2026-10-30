//! The shell's marks: what a shell integration says about itself in the stream, which no
//! terminal keeps.
//!
//! A shell that is integrated with its terminal writes sequences the emulator drops: where a
//! prompt starts and ends, where a command starts and how it ended (OSC 133, the semantic
//! prompt marks), the working directory (OSC 7), and bateri's dock (OSC 8133: the line being
//! edited, that the shell's editing widget is bound, the branch). The terminal reading the
//! stream builds its blocks, its prompt and its dock from them; an image without them comes
//! back as text with none of that, until the shell happens to print its next prompt. So the
//! screen follows the stream for them (see [`crate::tail::Tail`], which calls [`Marks::osc`]
//! where the parser dispatches an OSC) and the image writes them again, **in the order they
//! came**, after everything else: a terminal fed the image reads the same story from them,
//! told again.
//!
//! **What is kept** is the state the marks leave, not every mark:
//!
//! - OSC 133: every mark, oldest first, the newest [`PROMPT_BYTES`] of them. A block is told
//!   by its marks together (its prompt, its command, its end), and the blocks whose rows the
//!   image carries need theirs; a thousand commands or so fit.
//! - OSC 7: the last directory each host reported (the authority of the `file://` URL names
//!   the host; a shell behind ssh reports its own), the newest [`DIRECTORY_HOSTS`] hosts.
//! - OSC 8133: the last mark of each kind that says what is so — `u` (the line), `e` (the
//!   line is finished), `o` (the line does not fit), `b` (the branch), `w` (the widget is
//!   bound), `g` (the line is read from the grid) — and none of the kinds that say what
//!   happened once (a remote setup's fault or word: `f`, `i`). The fields of the form
//!   `name=value` before the kind say whose mark it is (bateri's remote shell names itself
//!   with `bt_remote=`), so the last of a kind is kept for each such name apart from the
//!   others; the value is not part of the name.
//!
//! A mark longer than its limit ([`MARK_LIMIT`], [`DOCK_LIMIT`]) is not one a terminal takes
//! and is not kept; one of a kind where only the last counts takes the kind's last away, so
//! that an older one is not told again as if it were the newest.
//!
//! **Neither a full reset (`ESC c`) nor a forget of the history touches them**: a reset is the
//! terminal's, and the shell's state is what it was; a forgotten row takes its block's marks
//! nowhere a terminal would draw them (a block whose rows are gone is drawn in none).
//!
//! The rules of the framing are the parser's ([`crate::tail::Tail`]: `BEL`, `CAN`, `SUB` and
//! `ESC` end the sequence and dispatch it; the C0 controls inside it are dropped), so the marks
//! are the ones a terminal reading the same stream would see. The number is the sequence's
//! first field exactly as written.

use std::collections::VecDeque;

/// The semantic prompt marks' number.
const PROMPT: &[u8] = b"133";

/// The working directory's number.
const DIRECTORY: &[u8] = b"7";

/// bateri's dock's number.
const DOCK: &[u8] = b"8133";

/// How many bytes of OSC 133 marks are kept, the newest: about four marks a command and
/// thirty bytes a mark make it a thousand commands or so. A design constant: past it the
/// oldest blocks come back without their marks, which is what a terminal that keeps fewer
/// blocks does anyway.
pub(crate) const PROMPT_BYTES: usize = 128 * 1024;

/// The longest OSC 133 mark kept (its payload, the number included). The marks are a letter
/// and a few fields; bateri reads none longer than 256 bytes after the number.
pub(crate) const MARK_LIMIT: usize = 512;

/// The longest OSC 8133 mark kept: bateri's own bound on the line's mark (64 KiB) and room for
/// the number and the fields that name a remote shell.
pub(crate) const DOCK_LIMIT: usize = 64 * 1024 + 512;

/// The longest OSC 7 kept: a path of the system's longest, percent-encoded, and a host.
pub(crate) const DIRECTORY_LIMIT: usize = 16 * 1024;

/// How many hosts' directories are kept, the newest.
pub(crate) const DIRECTORY_HOSTS: usize = 8;

/// The OSC 8133 kinds that say what is so; the others say what happened once.
const DOCK_KINDS: &[u8] = b"ueobwg";

/// One mark: when it came and its payload (the bytes between `ESC ]` and the end, the C0
/// controls left out, as the parser has them).
#[derive(Clone, Debug, PartialEq, Eq)]
struct Mark {
    at: u64,
    payload: Box<[u8]>,
}

/// What a mark of which only the last counts is the last of.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Kind {
    /// OSC 7, by the host it names (the URL's authority).
    Directory(Box<[u8]>),
    /// OSC 8133, by the names of the fields before its kind, and the kind.
    Dock(Box<[u8]>, u8),
}

/// The marks the stream left.
#[derive(Clone, Debug, Default)]
pub(crate) struct Marks {
    /// Every OSC 133 mark kept, oldest first.
    prompts: VecDeque<Mark>,
    /// The payload bytes in [`Marks::prompts`].
    prompt_bytes: usize,
    /// The last mark of each kind of which only the last counts.
    last: Vec<(Kind, Mark)>,
    /// The order of the next mark.
    next: u64,
}

impl PartialEq for Marks {
    /// Two are alike when they would write the same: the same marks in the same order.
    fn eq(&self, other: &Self) -> bool {
        self.in_order() == other.in_order()
    }
}

impl Eq for Marks {}

impl Marks {
    /// An OSC the parser dispatches, its payload as the parser has it.
    pub(crate) fn osc(&mut self, payload: &[u8]) {
        let number = payload
            .split(|&byte| byte == b';')
            .next()
            .unwrap_or_default();
        if number == PROMPT {
            self.prompt(payload);
        } else if number == DIRECTORY {
            let host = authority(payload.get(DIRECTORY.len() + 1..).unwrap_or_default());
            self.keep_last(Kind::Directory(host.into()), payload, DIRECTORY_LIMIT);
            self.bound_directories();
        } else if number == DOCK
            && let Some((names, kind)) =
                dock_kind(payload.get(DOCK.len() + 1..).unwrap_or_default())
        {
            self.keep_last(Kind::Dock(names.into(), kind), payload, DOCK_LIMIT);
        }
    }

    fn stamp(&mut self) -> u64 {
        let at = self.next;
        self.next += 1;
        at
    }

    fn prompt(&mut self, payload: &[u8]) {
        if payload.len() > MARK_LIMIT {
            return;
        }
        let at = self.stamp();
        self.prompt_bytes += payload.len();
        self.prompts.push_back(Mark {
            at,
            payload: payload.into(),
        });
        while self.prompt_bytes > PROMPT_BYTES {
            let Some(oldest) = self.prompts.pop_front() else {
                break;
            };
            self.prompt_bytes -= oldest.payload.len();
        }
    }

    /// `payload` is the newest of `kind`: the one before goes, and when this one is too long
    /// to keep, no mark of the kind is left.
    fn keep_last(&mut self, kind: Kind, payload: &[u8], limit: usize) {
        self.last.retain(|(kept, _)| *kept != kind);
        if payload.len() > limit {
            return;
        }
        let at = self.stamp();
        self.last.push((
            kind,
            Mark {
                at,
                payload: payload.into(),
            },
        ));
    }

    /// Only the newest [`DIRECTORY_HOSTS`] hosts' directories stay.
    fn bound_directories(&mut self) {
        let mut hosts: Vec<u64> = self
            .last
            .iter()
            .filter(|(kind, _)| matches!(kind, Kind::Directory(_)))
            .map(|(_, mark)| mark.at)
            .collect();
        let Some(over) = hosts
            .len()
            .checked_sub(DIRECTORY_HOSTS)
            .filter(|&over| over > 0)
        else {
            return;
        };
        hosts.sort_unstable();
        let oldest = hosts.get(..over).unwrap_or_default();
        self.last.retain(|(_, mark)| !oldest.contains(&mark.at));
    }

    /// The payloads kept, in the order they came.
    pub(crate) fn in_order(&self) -> Vec<&[u8]> {
        let mut marks: Vec<&Mark> = self
            .prompts
            .iter()
            .chain(self.last.iter().map(|(_, mark)| mark))
            .collect();
        marks.sort_by_key(|mark| mark.at);
        marks.into_iter().map(|mark| &*mark.payload).collect()
    }

    /// The marks as the sequences that tell them, in the order they came; nothing when there
    /// are none.
    pub(crate) fn write(&self, out: &mut Vec<u8>) {
        for payload in self.in_order() {
            out.extend_from_slice(b"\x1b]");
            out.extend_from_slice(payload);
            out.push(0x07);
        }
    }
}

/// The host an OSC 7 URL names: what is between `file://` and the path; the whole field when
/// it is not such a URL (a report that is not one is kept by what it says).
fn authority(url: &[u8]) -> &[u8] {
    let Some(rest) = url.strip_prefix(b"file://") else {
        return url;
    };
    let end = rest
        .iter()
        .position(|&byte| byte == b'/')
        .unwrap_or(rest.len());
    rest.get(..end).unwrap_or_default()
}

/// An OSC 8133 mark's kind and the names before it (`bt_remote=1.2.3;u;…` → `bt_remote=`,
/// `u`), when it is a kind that says what is so.
fn dock_kind(fields: &[u8]) -> Option<(Vec<u8>, u8)> {
    let mut names = Vec::new();
    for field in fields.split(|&byte| byte == b';') {
        if let Some(equals) = field.iter().position(|&byte| byte == b'=') {
            names.extend_from_slice(field.get(..=equals).unwrap_or_default());
            continue;
        }
        return match field {
            [kind] if DOCK_KINDS.contains(kind) => Some((names, *kind)),
            _ => None,
        };
    }
    None
}
