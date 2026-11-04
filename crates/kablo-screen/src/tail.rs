//! The sequence a read stopped in, the two modes the parser does not keep, and the shell's
//! marks.

use crate::kept::Kept;
use crate::marks::Marks;
use crate::skim;

/// Where the parser stands, as far as [`Tail`] needs: vte's states folded into the ones
/// that end a sequence alike (`vte-0.15.0/src/lib.rs`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Seq {
    #[default]
    Ground,
    Escape,
    EscapeIntermediate,
    /// Any CSI state: every one ends on `0x40..=0x7E`.
    Csi,
    /// DCS before its final byte: vte's entry, parameter and intermediate states, apart
    /// because they go to the ignored state on different bytes.
    DcsEntry,
    DcsParam,
    DcsIntermediate,
    /// DCS passthrough: CAN, SUB, ESC and the 8-bit ST end it.
    DcsBody,
    /// SOS/PM and an ignored DCS: only CAN, SUB and ESC end them — not `0x9c`, which a
    /// UTF-8 payload carries as a continuation byte.
    Ignored,
    /// An application program command (`ESC _`), which the parser hands to the handler:
    /// like [`Seq::Ignored`], but its `ESC` does not end it by itself.
    Apc,
    /// An `ESC` in an application program command: `\` completes it, any other byte
    /// cancels it and is read as the byte after an `ESC` that starts afresh.
    ApcEscape,
    Osc,
}

/// How many bytes of capacity [`Tail`] keeps once a sequence ends — one large OSC 52 must
/// not pin its size for the session's life.
const TAIL_KEEP: usize = 4096;

/// The most bytes of an application program command [`Tail`] keeps: 64 MiB of payload and
/// room for the command's header, the largest graphics command bateri takes (past it that
/// terminal drops the command anyway, so nothing a receiver would use is cut). A command
/// the receiver would take must reach it whole: cut short, its body would miss the middle
/// and the picture it carries would come out broken. The memory is held only while such a
/// command is open and goes back down to [`TAIL_KEEP`] when it ends.
const APC_KEEP: usize = (64 << 20) + 4096;

/// The bytes since the parser last stood in its **ground state**: a read that ended
/// inside a CSI/OSC/DCS or inside one character's UTF-8 left them with no effect on
/// `Term` yet — vte dispatches only at the final byte — so the image carries them
/// **after** the state and a fresh parser picks the sequence up where the old one stood.
///
/// vte does not say where it stands; this follows the same bytes. The C0 controls vte
/// **executes** inside an escape or a CSI are left out — they already took effect.
///
/// The same walk sees every sequence end, so it is also where [`Kept`] follows the modes
/// `alacritty_terminal` drops (`?1016`, `?2048`): a CSI that ends tells it, and so does
/// `ESC c`. Fed an image that wrote them, a new `Tail` is back where the old one was. Every
/// OSC that ends goes to [`Marks`] the same way, as the parser dispatches it.
///
/// **Known limit:** the cut is per code point, not per cluster: an emoji sequence cut
/// **between** two code points comes back as two clusters in a terminal that clusters
/// (the relay's own `Term` does not).
#[derive(Debug)]
pub(crate) struct Tail {
    state: Seq,
    /// UTF-8 continuation bytes still missing in the ground state.
    utf8_need: u8,
    bytes: Vec<u8>,
    /// The modes the program turned on that `Term` has no bit for.
    kept: Kept,
    /// The shell's marks the stream left.
    marks: Marks,
    /// How many bytes an application program command may hold ([`APC_KEEP`]; a test
    /// makes it small).
    apc_keep: usize,
}

impl Default for Tail {
    fn default() -> Self {
        Self {
            state: Seq::default(),
            utf8_need: 0,
            bytes: Vec::new(),
            kept: Kept::default(),
            marks: Marks::default(),
            apc_keep: APC_KEEP,
        }
    }
}

impl Tail {
    /// Follows `bytes`, which the parser gets next and in this order.
    ///
    /// What [`Tail::skim`] can take at once it takes at once, the rest is stepped through
    /// a byte at a time: every byte the relay reads comes through here, and most of it is
    /// text or the body of a sequence.
    pub(crate) fn feed(&mut self, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            let taken = self.skim(bytes);
            bytes = bytes.get(taken..).unwrap_or_default();
            if let Some((&byte, rest)) = bytes.split_first() {
                self.step(byte);
                bytes = rest;
            }
        }
    }

    /// Takes the bytes from the start of `bytes` that leave the state where it is, keeping
    /// of them what [`Tail::step`] would keep, and says how many it took: 0 when the next
    /// byte is one to step through. The same as stepping through them one by one.
    fn skim(&mut self, bytes: &[u8]) -> usize {
        match self.state {
            Seq::Ground if self.utf8_need == 0 => ground_run(bytes),
            Seq::Apc => {
                let run = skim::until(bytes, |byte| matches!(byte, 0x18 | 0x1a | 0x1b));
                let room = self.apc_keep.saturating_sub(self.bytes.len());
                self.bytes
                    .extend_from_slice(bytes.get(..run.min(room)).unwrap_or_default());
                run
            }
            // Every byte from 0x20 up is the OSC's; below it, BEL, CAN, SUB and ESC end it
            // and the other controls are dropped, all of which the step reads.
            Seq::Osc => {
                let run = skim::until(bytes, |byte| byte < 0x20);
                self.bytes
                    .extend_from_slice(bytes.get(..run).unwrap_or_default());
                run
            }
            Seq::Ignored => {
                let run = skim::until(bytes, |byte| matches!(byte, 0x18 | 0x1a | 0x1b));
                self.bytes
                    .extend_from_slice(bytes.get(..run).unwrap_or_default());
                run
            }
            Seq::DcsBody => {
                let run = skim::until(bytes, |byte| matches!(byte, 0x18 | 0x1a | 0x1b | 0x9c));
                self.bytes
                    .extend_from_slice(bytes.get(..run).unwrap_or_default());
                run
            }
            _ => 0,
        }
    }

    /// The bytes of the sequence (or the character) the stream stopped in; empty in the
    /// ground state.
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The modes the stream turned on that `Term` does not keep.
    pub(crate) fn kept(&self) -> Kept {
        self.kept
    }

    /// The shell's marks the stream left.
    pub(crate) fn marks(&self) -> &Marks {
        &self.marks
    }

    /// The OSC open in [`Tail::bytes`] ends here and the parser dispatches it: its payload is
    /// what follows `ESC ]`.
    fn dispatch_osc(&mut self) {
        self.marks.osc(self.bytes.get(2..).unwrap_or_default());
    }

    fn ground(&mut self) {
        self.state = Seq::Ground;
        self.utf8_need = 0;
        self.bytes.clear();
        if self.bytes.capacity() > TAIL_KEEP {
            self.bytes.shrink_to(TAIL_KEEP);
        }
    }

    /// ESC from the ground or inside any sequence: a **new** sequence.
    fn escape(&mut self) {
        self.ground();
        self.state = Seq::Escape;
        self.bytes.push(0x1b);
    }

    /// The bytes vte handles the same in every state (its `anywhere`).
    fn anywhere(&mut self, byte: u8) {
        match byte {
            0x18 | 0x1a => self.ground(),
            0x1b => self.escape(),
            _ => {}
        }
    }

    fn enter(&mut self, byte: u8, state: Seq) {
        self.bytes.push(byte);
        self.state = state;
    }

    fn step(&mut self, byte: u8) {
        const EXECUTED: [std::ops::RangeInclusive<u8>; 3] = [0x00..=0x17, 0x19..=0x19, 0x1c..=0x1f];
        let executed = EXECUTED.iter().any(|range| range.contains(&byte));
        match self.state {
            Seq::Ground => {
                if byte == 0x1b {
                    return self.escape();
                }
                if self.utf8_need > 0 {
                    if byte & 0xc0 == 0x80 {
                        self.bytes.push(byte);
                        self.utf8_need -= 1;
                        if self.utf8_need == 0 {
                            self.ground();
                        }
                        return;
                    }
                    // Not a continuation: vte prints U+FFFD and reads this byte afresh.
                    self.ground();
                }
                let need = match byte {
                    0xc2..=0xdf => 1,
                    0xe0..=0xef => 2,
                    0xf0..=0xf4 => 3,
                    _ => 0,
                };
                if need > 0 {
                    self.bytes.push(byte);
                    self.utf8_need = need;
                }
            }
            Seq::Escape => match byte {
                0x18 | 0x1a => self.ground(),
                // A second ESC is ignored, not a restart (vte's `advance_esc`).
                0x1b => {}
                _ if executed => {}
                0x20..=0x2f => self.enter(byte, Seq::EscapeIntermediate),
                0x50 => self.enter(byte, Seq::DcsEntry),
                0x58 | 0x5e => self.enter(byte, Seq::Ignored),
                0x5f => self.enter(byte, Seq::Apc),
                0x5b => self.enter(byte, Seq::Csi),
                0x5d => self.enter(byte, Seq::Osc),
                // `ESC c`, a full reset (no intermediate: that is another sequence).
                0x63 => {
                    self.kept.reset();
                    self.ground();
                }
                0x30..=0x7e => self.ground(),
                _ => {}
            },
            Seq::EscapeIntermediate => match byte {
                _ if executed => {}
                0x20..=0x2f => self.bytes.push(byte),
                0x30..=0x7e => self.ground(),
                _ => self.anywhere(byte),
            },
            Seq::Csi => match byte {
                _ if executed => {}
                0x20..=0x3f => self.bytes.push(byte),
                0x40..=0x7e => {
                    self.kept.csi(&self.bytes, byte);
                    self.ground();
                }
                _ => self.anywhere(byte),
            },
            Seq::DcsEntry => match byte {
                0x20..=0x2f => self.enter(byte, Seq::DcsIntermediate),
                0x30..=0x3f => self.enter(byte, Seq::DcsParam),
                0x40..=0x7e => self.enter(byte, Seq::DcsBody),
                _ => self.anywhere(byte),
            },
            Seq::DcsParam => match byte {
                0x20..=0x2f => self.enter(byte, Seq::DcsIntermediate),
                0x30..=0x3b => self.bytes.push(byte),
                0x3c..=0x3f => self.enter(byte, Seq::Ignored),
                0x40..=0x7e => self.enter(byte, Seq::DcsBody),
                _ => self.anywhere(byte),
            },
            Seq::DcsIntermediate => match byte {
                0x20..=0x2f => self.bytes.push(byte),
                0x30..=0x3f => self.enter(byte, Seq::Ignored),
                0x40..=0x7e => self.enter(byte, Seq::DcsBody),
                _ => self.anywhere(byte),
            },
            Seq::Ignored => match byte {
                0x18 | 0x1a => self.ground(),
                0x1b => self.escape(),
                _ => self.bytes.push(byte),
            },
            Seq::Apc => match byte {
                0x18 | 0x1a => self.ground(),
                // The command stays open until the next byte says how it ends.
                0x1b => self.enter(byte, Seq::ApcEscape),
                _ if self.bytes.len() < self.apc_keep => self.bytes.push(byte),
                _ => {}
            },
            Seq::ApcEscape => {
                if byte == 0x5c {
                    self.ground();
                } else {
                    // Cancelled; this byte follows an `ESC` that starts afresh.
                    self.escape();
                    self.step(byte);
                }
            }
            Seq::DcsBody => match byte {
                0x18 | 0x1a | 0x9c => self.ground(),
                0x1b => self.escape(),
                _ => self.bytes.push(byte),
            },
            // All four end the sequence and dispatch it (vte's `osc_end`).
            Seq::Osc => match byte {
                0x07 | 0x18 | 0x1a => {
                    self.dispatch_osc();
                    self.ground();
                }
                0x1b => {
                    self.dispatch_osc();
                    self.escape();
                }
                // Ignored inside an OSC.
                _ if executed => {}
                _ => self.bytes.push(byte),
            },
        }
    }
}

/// How far the ground state goes on unmoved, with no character half read: ASCII other than
/// `ESC`, the bytes no character starts with, and whole characters of UTF-8, each of which
/// [`Tail::step`] takes a byte at a time and comes back to the ground from with nothing
/// kept. It stops at an `ESC` and at a character that is cut or malformed, which the step
/// reads.
fn ground_run(bytes: &[u8]) -> usize {
    let mut at = 0;
    loop {
        let rest = bytes.get(at..).unwrap_or_default();
        at += skim::until(rest, |byte| byte == 0x1b || byte >= 0x80);
        let need = match bytes.get(at) {
            None | Some(0x1b) => return at,
            Some(0xc2..=0xdf) => 1,
            Some(0xe0..=0xef) => 2,
            Some(0xf0..=0xf4) => 3,
            Some(_) => 0,
        };
        match bytes.get(at + 1..=at + need) {
            Some(continuation) if continuation.iter().all(|&byte| byte & 0xc0 == 0x80) => {
                at += 1 + need;
            }
            _ => return at,
        }
    }
}

#[cfg(test)]
impl Tail {
    pub(crate) fn capacity(&self) -> usize {
        self.bytes.capacity()
    }

    /// A tail whose application program commands are cut at `apc_keep` bytes.
    pub(crate) fn with_apc_keep(apc_keep: usize) -> Self {
        Self {
            apc_keep,
            ..Self::default()
        }
    }
}

#[cfg(test)]
pub(crate) const KEEP_LIMITS: (usize, usize) = (TAIL_KEEP, APC_KEEP);

#[cfg(test)]
mod tests {
    use super::*;

    /// Everything a tail stands for, as one value to compare.
    fn state(tail: &Tail) -> (Seq, u8, Vec<u8>, Kept, String) {
        (
            tail.state,
            tail.utf8_need,
            tail.bytes.clone(),
            tail.kept,
            format!("{:?}", tail.marks),
        )
    }

    /// A stream that visits every state: text, whole and cut UTF-8, CSI that moves a mode,
    /// OSC with marks, APC, DCS, SOS, and the bytes that cancel or restart each.
    fn mixed() -> Vec<u8> {
        let mut stream = Vec::new();
        for part in [
            "plain text\r\n\tand more \u{e7}ay \u{11f} \u{65e6}\u{6d77} \u{1f44d}",
            "\x1b[1;31mred\x1b[0m\x1b[?1016h\x1b[?2048h\x1b[2J\x1b[?1016l\x1b[38:5:24m",
            "\x1b]0;a title\x07\x1b]2;another\x1b\\\x1b]133;A\x07text\x1b]133;D;0\x07\x1b]7;file://h/p\x1b\\",
            "\x1b]52;c;aGVsbG8=\x18after a cancel\x1b]6;\x01\x02ignored\x7f\x07",
            "\x1b_Ga=t,m=1;QUJD\x1b\\\x1b_Gm=0;REVG\x1b\\\x1b_G\x1bcancelled\x1b_Gq;\x1a",
            "\x1bP1$qm\x1b\\\x1bPq#0;1;2\u{9c}\x1bX sos body\x1b\\\x1b^ pm\x18x",
            "\x1bc\x1b(B\x1b)0\x1b#8\x1b \x1b\x1b[\x1b]",
        ] {
            stream.extend_from_slice(part.as_bytes());
        }
        // A character cut off by an escape and by a plain byte, and stray continuations.
        stream.extend_from_slice(b"\xe6\x97\x1b[m\xf0\x9f a\x80\xbf\xc2\xc2\xa9\xff\xf5z");
        // Enough text on each side of a block edge for the block walk to run.
        stream.extend_from_slice(&b"0123456789abcdef".repeat(2));
        stream.extend_from_slice(b"\x1b]2;");
        stream.extend_from_slice(&b"0123456789abcdef".repeat(2));
        stream.extend_from_slice(b"\x07\x1bP1$q");
        stream.extend_from_slice(&b"0123456789abcdef".repeat(2));
        stream.extend_from_slice(b"\x1b\\");
        stream
    }

    /// Skimming is stepping: however the stream is cut into reads, one cut or two, the tail
    /// is the one a walk of its bytes one by one leaves.
    #[test]
    fn skimming_a_read_leaves_the_tail_stepping_through_it_leaves() {
        let stream = mixed();
        let step = |tail: &mut Tail, bytes: &[u8]| {
            for &byte in bytes {
                tail.step(byte);
            }
        };
        let mut whole = Tail::default();
        whole.feed(&stream);
        let mut stepped = Tail::default();
        step(&mut stepped, &stream);
        assert_eq!(state(&whole), state(&stepped));
        for first in 0..=stream.len() {
            let (head, rest) = stream.split_at(first);
            let (mut skimmed, mut walked) = (Tail::default(), Tail::default());
            skimmed.feed(head);
            step(&mut walked, head);
            assert_eq!(state(&skimmed), state(&walked), "one cut at {first}");
            for second in first..=stream.len() {
                let (mut skimmed, mut walked) = (Tail::default(), Tail::default());
                let (middle, last) = rest.split_at(second - first);
                skimmed.feed(head);
                skimmed.feed(middle);
                step(&mut walked, head);
                step(&mut walked, middle);
                assert_eq!(
                    state(&skimmed),
                    state(&walked),
                    "cuts at {first} and {second}"
                );
                skimmed.feed(last);
                step(&mut walked, last);
                assert_eq!(
                    state(&skimmed),
                    state(&walked),
                    "cuts at {first} and {second}, then the rest"
                );
            }
        }
    }

    /// An application program command past what the tail keeps is cut at the same byte
    /// whether it arrives whole or a byte at a time.
    #[test]
    fn a_command_past_its_cap_is_cut_where_stepping_cuts_it() {
        let mut stream = b"\x1b_G".to_vec();
        stream.extend_from_slice(&b"0123456789abcdef".repeat(8));
        for cap in [0, 3, 40, 70, 400] {
            let (mut skimmed, mut walked) = (Tail::with_apc_keep(cap), Tail::with_apc_keep(cap));
            skimmed.feed(&stream);
            for &byte in &stream {
                walked.step(byte);
            }
            assert_eq!(state(&skimmed), state(&walked), "cap {cap}");
        }
    }
}
