//! The sequence a read stopped in.

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
/// **Known limit:** the cut is per code point, not per cluster: an emoji sequence cut
/// **between** two code points comes back as two clusters in a terminal that clusters
/// (the relay's own `Term` does not).
#[derive(Debug)]
pub(crate) struct Tail {
    state: Seq,
    /// UTF-8 continuation bytes still missing in the ground state.
    utf8_need: u8,
    bytes: Vec<u8>,
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
            apc_keep: APC_KEEP,
        }
    }
}

impl Tail {
    /// Follows `bytes`, which the parser gets next and in this order.
    pub(crate) fn feed(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.step(byte);
        }
    }

    /// The bytes of the sequence (or the character) the stream stopped in; empty in the
    /// ground state.
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
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
                0x40..=0x7e => self.ground(),
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
            Seq::Osc => match byte {
                0x07 | 0x18 | 0x1a => self.ground(),
                0x1b => self.escape(),
                // Ignored inside an OSC.
                _ if executed => {}
                _ => self.bytes.push(byte),
            },
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
