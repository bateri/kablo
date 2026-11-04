//! Parser for implementing virtual terminal emulators
//!
//! [`Parser`] is implemented according to [Paul Williams' ANSI parser state
//! machine]. The state machine doesn't assign meaning to the parsed data and is
//! thus not itself sufficient for writing a terminal emulator. Instead, it is
//! expected that an implementation of [`Perform`] is provided which does
//! something useful with the parsed data. The [`Parser`] handles the book
//! keeping, and the [`Perform`] gets to simply handle actions.
//!
//! # Examples
//!
//! For an example of using the [`Parser`] please see the examples folder. The
//! example included there simply logs all the actions [`Perform`] does. One
//! quick way to see it in action is to pipe `printf` into it
//!
//! ```sh
//! printf '\x1b[31mExample' | cargo run --example parselog
//! ```
//!
//! # Differences from original state machine description
//!
//! * UTF-8 Support for Input
//! * OSC Strings can be terminated by 0x07
//! * Only supports 7-bit codes
//! * Application program commands (`ESC _ … ESC \`) are passed on in pieces, see
//!   [`Perform::apc_start`]; the state machine drops the other two string types
//!   (SOS and PM)
//!
//! [`Parser`]: struct.Parser.html
//! [`Perform`]: trait.Perform.html
//! [Paul Williams' ANSI parser state machine]: https://vt100.net/emu/dec_ansi_parser
#![deny(clippy::all, clippy::if_not_else, clippy::enum_glob_use)]
#![cfg_attr(not(feature = "std"), no_std)]

use core::mem::MaybeUninit;
use core::str;

#[cfg(not(feature = "std"))]
use arrayvec::ArrayVec;

mod params;

#[cfg(feature = "ansi")]
pub mod ansi;
pub use params::{Params, ParamsIter};

const MAX_INTERMEDIATES: usize = 2;
const MAX_OSC_PARAMS: usize = 16;
const MAX_OSC_RAW: usize = 1024;

/// Parser for raw _VTE_ protocol which delegates actions to a [`Perform`]
///
/// [`Perform`]: trait.Perform.html
///
/// Generic over the value for the size of the raw Operating System Command
/// buffer. Only used when the `std` feature is not enabled.
#[derive(Default)]
pub struct Parser<const OSC_RAW_BUF_SIZE: usize = MAX_OSC_RAW> {
    state: State,
    intermediates: [u8; MAX_INTERMEDIATES],
    intermediate_idx: usize,
    params: Params,
    param: u16,
    #[cfg(not(feature = "std"))]
    osc_raw: ArrayVec<u8, OSC_RAW_BUF_SIZE>,
    #[cfg(feature = "std")]
    osc_raw: Vec<u8>,
    osc_params: [(usize, usize); MAX_OSC_PARAMS],
    osc_num_params: usize,
    ignoring: bool,
    partial_utf8: [u8; 4],
    partial_utf8_len: usize,
}

impl Parser {
    /// Create a new Parser
    pub fn new() -> Parser {
        Default::default()
    }
}

impl<const OSC_RAW_BUF_SIZE: usize> Parser<OSC_RAW_BUF_SIZE> {
    /// Create a new Parser with a custom size for the Operating System Command
    /// buffer.
    ///
    /// Call with a const-generic param on `Parser`, like:
    ///
    /// ```rust
    /// let mut p = vte::Parser::<64>::new_with_size();
    /// ```
    #[cfg(not(feature = "std"))]
    pub fn new_with_size() -> Parser<OSC_RAW_BUF_SIZE> {
        Default::default()
    }

    #[inline]
    fn params(&self) -> &Params {
        &self.params
    }

    #[inline]
    fn intermediates(&self) -> &[u8] {
        &self.intermediates[..self.intermediate_idx]
    }

    /// Advance the parser state.
    ///
    /// Requires a [`Perform`] implementation to handle the triggered actions.
    ///
    /// [`Perform`]: trait.Perform.html
    #[inline]
    pub fn advance<P: Perform>(&mut self, performer: &mut P, bytes: &[u8]) {
        let mut i = 0;

        // Handle partial codepoints from previous calls to `advance`.
        if self.partial_utf8_len != 0 {
            i += self.advance_partial_utf8(performer, bytes);
        }

        while i != bytes.len() {
            match self.state {
                State::Ground => i += self.advance_ground(performer, &bytes[i..]),
                State::ApcString => i += self.advance_apc(performer, &bytes[i..]),
                _ => {
                    // Inlining it results in worse codegen.
                    let byte = bytes[i];
                    self.change_state(performer, byte);
                    i += 1;
                },
            }
        }
    }

    /// Partially advance the parser state.
    ///
    /// This is equivalent to [`Self::advance`], but stops when
    /// [`Perform::terminated`] is true after reading a byte.
    ///
    /// Returns the number of bytes read before termination.
    ///
    /// See [`Perform::advance`] for more details.
    #[inline]
    #[must_use = "Returned value should be used to processs the remaining bytes"]
    pub fn advance_until_terminated<P: Perform>(
        &mut self,
        performer: &mut P,
        bytes: &[u8],
    ) -> usize {
        let mut i = 0;

        // Handle partial codepoints from previous calls to `advance`.
        if self.partial_utf8_len != 0 {
            i += self.advance_partial_utf8(performer, bytes);
        }

        while i != bytes.len() && !performer.terminated() {
            match self.state {
                State::Ground => i += self.advance_ground(performer, &bytes[i..]),
                State::ApcString => i += self.advance_apc(performer, &bytes[i..]),
                _ => {
                    // Inlining it results in worse codegen.
                    let byte = bytes[i];
                    self.change_state(performer, byte);
                    i += 1;
                },
            }
        }

        i
    }

    #[inline(always)]
    fn change_state<P: Perform>(&mut self, performer: &mut P, byte: u8) {
        match self.state {
            State::CsiEntry => self.advance_csi_entry(performer, byte),
            State::CsiIgnore => self.advance_csi_ignore(performer, byte),
            State::CsiIntermediate => self.advance_csi_intermediate(performer, byte),
            State::CsiParam => self.advance_csi_param(performer, byte),
            State::DcsEntry => self.advance_dcs_entry(performer, byte),
            State::DcsIgnore => self.anywhere(performer, byte),
            State::DcsIntermediate => self.advance_dcs_intermediate(performer, byte),
            State::DcsParam => self.advance_dcs_param(performer, byte),
            State::DcsPassthrough => self.advance_dcs_passthrough(performer, byte),
            State::Escape => self.advance_esc(performer, byte),
            State::EscapeIntermediate => self.advance_esc_intermediate(performer, byte),
            State::OscString => self.advance_osc_string(performer, byte),
            State::SosPmApcString => self.anywhere(performer, byte),
            State::ApcString => {
                // The loops hand this state whole slices; a single byte is the same slice.
                self.advance_apc(performer, core::slice::from_ref(&byte));
            },
            State::ApcEscape => self.advance_apc_escape(performer, byte),
            State::Ground => unreachable!(),
        }
    }

    #[inline(always)]
    fn advance_csi_entry<P: Perform>(&mut self, performer: &mut P, byte: u8) {
        match byte {
            0x00..=0x17 | 0x19 | 0x1C..=0x1F => performer.execute(byte),
            0x20..=0x2F => {
                self.action_collect(byte);
                self.state = State::CsiIntermediate
            },
            0x30..=0x39 => {
                self.action_paramnext(byte);
                self.state = State::CsiParam
            },
            0x3A => {
                self.action_subparam();
                self.state = State::CsiParam
            },
            0x3B => {
                self.action_param();
                self.state = State::CsiParam
            },
            0x3C..=0x3F => {
                self.action_collect(byte);
                self.state = State::CsiParam
            },
            0x40..=0x7E => self.action_csi_dispatch(performer, byte),
            _ => self.anywhere(performer, byte),
        }
    }

    #[inline(always)]
    fn advance_csi_ignore<P: Perform>(&mut self, performer: &mut P, byte: u8) {
        match byte {
            0x00..=0x17 | 0x19 | 0x1C..=0x1F => performer.execute(byte),
            0x20..=0x3F => (),
            0x40..=0x7E => self.state = State::Ground,
            0x7F => (),
            _ => self.anywhere(performer, byte),
        }
    }

    #[inline(always)]
    fn advance_csi_intermediate<P: Perform>(&mut self, performer: &mut P, byte: u8) {
        match byte {
            0x00..=0x17 | 0x19 | 0x1C..=0x1F => performer.execute(byte),
            0x20..=0x2F => self.action_collect(byte),
            0x30..=0x3F => self.state = State::CsiIgnore,
            0x40..=0x7E => self.action_csi_dispatch(performer, byte),
            _ => self.anywhere(performer, byte),
        }
    }

    #[inline(always)]
    fn advance_csi_param<P: Perform>(&mut self, performer: &mut P, byte: u8) {
        match byte {
            0x00..=0x17 | 0x19 | 0x1C..=0x1F => performer.execute(byte),
            0x20..=0x2F => {
                self.action_collect(byte);
                self.state = State::CsiIntermediate
            },
            0x30..=0x39 => self.action_paramnext(byte),
            0x3A => self.action_subparam(),
            0x3B => self.action_param(),
            0x3C..=0x3F => self.state = State::CsiIgnore,
            0x40..=0x7E => self.action_csi_dispatch(performer, byte),
            0x7F => (),
            _ => self.anywhere(performer, byte),
        }
    }

    #[inline(always)]
    fn advance_dcs_entry<P: Perform>(&mut self, performer: &mut P, byte: u8) {
        match byte {
            0x00..=0x17 | 0x19 | 0x1C..=0x1F => (),
            0x20..=0x2F => {
                self.action_collect(byte);
                self.state = State::DcsIntermediate
            },
            0x30..=0x39 => {
                self.action_paramnext(byte);
                self.state = State::DcsParam
            },
            0x3A => {
                self.action_subparam();
                self.state = State::DcsParam
            },
            0x3B => {
                self.action_param();
                self.state = State::DcsParam
            },
            0x3C..=0x3F => {
                self.action_collect(byte);
                self.state = State::DcsParam
            },
            0x40..=0x7E => self.action_hook(performer, byte),
            0x7F => (),
            _ => self.anywhere(performer, byte),
        }
    }

    #[inline(always)]
    fn advance_dcs_intermediate<P: Perform>(&mut self, performer: &mut P, byte: u8) {
        match byte {
            0x00..=0x17 | 0x19 | 0x1C..=0x1F => (),
            0x20..=0x2F => self.action_collect(byte),
            0x30..=0x3F => self.state = State::DcsIgnore,
            0x40..=0x7E => self.action_hook(performer, byte),
            0x7F => (),
            _ => self.anywhere(performer, byte),
        }
    }

    #[inline(always)]
    fn advance_dcs_param<P: Perform>(&mut self, performer: &mut P, byte: u8) {
        match byte {
            0x00..=0x17 | 0x19 | 0x1C..=0x1F => (),
            0x20..=0x2F => {
                self.action_collect(byte);
                self.state = State::DcsIntermediate
            },
            0x30..=0x39 => self.action_paramnext(byte),
            0x3A => self.action_subparam(),
            0x3B => self.action_param(),
            0x3C..=0x3F => self.state = State::DcsIgnore,
            0x40..=0x7E => self.action_hook(performer, byte),
            0x7F => (),
            _ => self.anywhere(performer, byte),
        }
    }

    #[inline(always)]
    fn advance_dcs_passthrough<P: Perform>(&mut self, performer: &mut P, byte: u8) {
        match byte {
            0x00..=0x17 | 0x19 | 0x1C..=0x7E => performer.put(byte),
            0x18 | 0x1A => {
                performer.unhook();
                performer.execute(byte);
                self.state = State::Ground
            },
            0x1B => {
                performer.unhook();
                self.reset_params();
                self.state = State::Escape
            },
            0x7F => (),
            0x9C => {
                performer.unhook();
                self.state = State::Ground
            },
            _ => (),
        }
    }

    #[inline(always)]
    fn advance_esc<P: Perform>(&mut self, performer: &mut P, byte: u8) {
        match byte {
            0x00..=0x17 | 0x19 | 0x1C..=0x1F => performer.execute(byte),
            0x20..=0x2F => {
                self.action_collect(byte);
                self.state = State::EscapeIntermediate
            },
            0x30..=0x4F => {
                performer.esc_dispatch(self.intermediates(), self.ignoring, byte);
                self.state = State::Ground
            },
            0x50 => {
                self.reset_params();
                self.state = State::DcsEntry
            },
            0x51..=0x57 => {
                performer.esc_dispatch(self.intermediates(), self.ignoring, byte);
                self.state = State::Ground
            },
            0x58 => self.state = State::SosPmApcString,
            0x59..=0x5A => {
                performer.esc_dispatch(self.intermediates(), self.ignoring, byte);
                self.state = State::Ground
            },
            0x5B => {
                self.reset_params();
                self.state = State::CsiEntry
            },
            0x5C => {
                performer.esc_dispatch(self.intermediates(), self.ignoring, byte);
                self.state = State::Ground
            },
            0x5D => {
                self.osc_raw.clear();
                self.osc_num_params = 0;
                self.state = State::OscString
            },
            0x5E => self.state = State::SosPmApcString,
            0x5F => {
                performer.apc_start();
                self.state = State::ApcString
            },
            0x60..=0x7E => {
                performer.esc_dispatch(self.intermediates(), self.ignoring, byte);
                self.state = State::Ground
            },
            // Anywhere.
            0x18 | 0x1A => {
                performer.execute(byte);
                self.state = State::Ground
            },
            0x1B => (),
            _ => (),
        }
    }

    #[inline(always)]
    fn advance_esc_intermediate<P: Perform>(&mut self, performer: &mut P, byte: u8) {
        match byte {
            0x00..=0x17 | 0x19 | 0x1C..=0x1F => performer.execute(byte),
            0x20..=0x2F => self.action_collect(byte),
            0x30..=0x7E => {
                performer.esc_dispatch(self.intermediates(), self.ignoring, byte);
                self.state = State::Ground
            },
            0x7F => (),
            _ => self.anywhere(performer, byte),
        }
    }

    #[inline(always)]
    fn advance_osc_string<P: Perform>(&mut self, performer: &mut P, byte: u8) {
        match byte {
            0x00..=0x06 | 0x08..=0x17 | 0x19 | 0x1C..=0x1F => (),
            0x07 => {
                self.osc_end(performer, byte);
                self.state = State::Ground
            },
            0x18 | 0x1A => {
                self.osc_end(performer, byte);
                performer.execute(byte);
                self.state = State::Ground
            },
            0x1B => {
                self.osc_end(performer, byte);
                self.reset_params();
                self.state = State::Escape
            },
            0x3B => {
                #[cfg(not(feature = "std"))]
                {
                    if self.osc_raw.is_full() {
                        return;
                    }
                }
                self.action_osc_put_param()
            },
            _ => self.action_osc_put(byte),
        }
    }

    #[inline(always)]
    fn anywhere<P: Perform>(&mut self, performer: &mut P, byte: u8) {
        match byte {
            0x18 | 0x1A => {
                performer.execute(byte);
                self.state = State::Ground
            },
            0x1B => {
                self.reset_params();
                self.state = State::Escape
            },
            _ => (),
        }
    }

    /// Advance the parser inside an application program command.
    ///
    /// Like the ground state this one is left only through bytes found by a search, so the
    /// body goes to the performer in one piece: everything up to the next `ESC`, `CAN` or
    /// `SUB`. `ESC` does not end the command by itself, the byte after it says how it ends
    /// (see [`Self::advance_apc_escape`]); `CAN` and `SUB` cancel it.
    #[inline]
    fn advance_apc<P: Perform>(&mut self, performer: &mut P, bytes: &[u8]) -> usize {
        match memchr::memchr3(0x18, 0x1A, 0x1B, bytes) {
            None => {
                performer.apc_put(bytes);
                bytes.len()
            },
            Some(end) => {
                if end != 0 {
                    performer.apc_put(&bytes[..end]);
                }

                if bytes[end] == 0x1B {
                    self.state = State::ApcEscape;
                } else {
                    performer.apc_end(true);
                    performer.execute(bytes[end]);
                    self.state = State::Ground;
                }

                end + 1
            },
        }
    }

    /// Advance the parser after an `ESC` inside an application program command.
    ///
    /// `ESC \` (the string terminator) completes the command. Any other byte cancels it and
    /// is handled as the byte after an `ESC` that starts afresh, so `ESC _` right after a
    /// command that never ended begins the next one.
    #[inline]
    fn advance_apc_escape<P: Perform>(&mut self, performer: &mut P, byte: u8) {
        if byte == 0x5C {
            performer.apc_end(false);
            self.state = State::Ground;
        } else {
            performer.apc_end(true);
            self.reset_params();
            self.state = State::Escape;
            self.advance_esc(performer, byte);
        }
    }

    #[inline]
    fn action_csi_dispatch<P: Perform>(&mut self, performer: &mut P, byte: u8) {
        if self.params.is_full() {
            self.ignoring = true;
        } else {
            self.params.push(self.param);
        }
        performer.csi_dispatch(self.params(), self.intermediates(), self.ignoring, byte as char);

        self.state = State::Ground
    }

    #[inline]
    fn action_hook<P: Perform>(&mut self, performer: &mut P, byte: u8) {
        if self.params.is_full() {
            self.ignoring = true;
        } else {
            self.params.push(self.param);
        }
        performer.hook(self.params(), self.intermediates(), self.ignoring, byte as char);
        self.state = State::DcsPassthrough;
    }

    #[inline]
    fn action_collect(&mut self, byte: u8) {
        if self.intermediate_idx == MAX_INTERMEDIATES {
            self.ignoring = true;
        } else {
            self.intermediates[self.intermediate_idx] = byte;
            self.intermediate_idx += 1;
        }
    }

    /// Advance to the next subparameter.
    #[inline]
    fn action_subparam(&mut self) {
        if self.params.is_full() {
            self.ignoring = true;
        } else {
            self.params.extend(self.param);
            self.param = 0;
        }
    }

    /// Advance to the next parameter.
    #[inline]
    fn action_param(&mut self) {
        if self.params.is_full() {
            self.ignoring = true;
        } else {
            self.params.push(self.param);
            self.param = 0;
        }
    }

    /// Advance inside the parameter without terminating it.
    #[inline]
    fn action_paramnext(&mut self, byte: u8) {
        if self.params.is_full() {
            self.ignoring = true;
        } else {
            // Continue collecting bytes into param.
            self.param = self.param.saturating_mul(10);
            self.param = self.param.saturating_add((byte - b'0') as u16);
        }
    }

    /// Add OSC param separator.
    #[inline]
    fn action_osc_put_param(&mut self) {
        let idx = self.osc_raw.len();

        let param_idx = self.osc_num_params;
        match param_idx {
            // First param is special - 0 to current byte index.
            0 => self.osc_params[param_idx] = (0, idx),

            // Only process up to MAX_OSC_PARAMS.
            MAX_OSC_PARAMS => return,

            // All other params depend on previous indexing.
            _ => {
                let prev = self.osc_params[param_idx - 1];
                let begin = prev.1;
                self.osc_params[param_idx] = (begin, idx);
            },
        }

        self.osc_num_params += 1;
    }

    #[inline(always)]
    fn action_osc_put(&mut self, byte: u8) {
        #[cfg(not(feature = "std"))]
        {
            if self.osc_raw.is_full() {
                return;
            }
        }
        self.osc_raw.push(byte);
    }

    fn osc_end<P: Perform>(&mut self, performer: &mut P, byte: u8) {
        self.action_osc_put_param();
        self.osc_dispatch(performer, byte);
        self.osc_raw.clear();
        self.osc_num_params = 0;
    }

    /// Reset escape sequence parameters and intermediates.
    #[inline]
    fn reset_params(&mut self) {
        self.intermediate_idx = 0;
        self.ignoring = false;
        self.param = 0;

        self.params.clear();
    }

    /// Separate method for osc_dispatch that borrows self as read-only
    ///
    /// The aliasing is needed here for multiple slices into self.osc_raw
    #[inline]
    fn osc_dispatch<P: Perform>(&self, performer: &mut P, byte: u8) {
        let mut slices: [MaybeUninit<&[u8]>; MAX_OSC_PARAMS] =
            unsafe { MaybeUninit::uninit().assume_init() };

        for (i, slice) in slices.iter_mut().enumerate().take(self.osc_num_params) {
            let indices = self.osc_params[i];
            *slice = MaybeUninit::new(&self.osc_raw[indices.0..indices.1]);
        }

        unsafe {
            let num_params = self.osc_num_params;
            let params = &slices[..num_params] as *const [MaybeUninit<&[u8]>] as *const [&[u8]];
            performer.osc_dispatch(&*params, byte == 0x07);
        }
    }

    /// Advance the parser state from ground.
    ///
    /// The ground state is handled separately since it can only be left using
    /// the escape character (`\x1b`). This allows more efficient parsing by
    /// using SIMD search with [`memchr`].
    #[inline]
    fn advance_ground<P: Perform>(&mut self, performer: &mut P, bytes: &[u8]) -> usize {
        // Text is dispatched while it is checked to be UTF-8, in one pass that also finds the
        // escape that ends it. What this does not take (invalid bytes, a character cut off by
        // the end of the read) is left to the checks of `str::from_utf8`, which have always
        // decided what becomes of it.
        let num_bytes = bytes.len();
        let done = Self::dispatch_text(performer, bytes);
        if done == num_bytes {
            return done;
        }

        // If the next character is ESC, just process it and short-circuit.
        if bytes[done] == 0x1B {
            self.state = State::Escape;
            self.reset_params();
            return done + 1;
        }

        let plain_chars = memchr::memchr(0x1B, &bytes[done..]).unwrap_or(num_bytes - done);
        done + self.advance_ground_checked(performer, &bytes[done..], plain_chars)
    }

    /// Advance the parser from ground over `bytes`, whose first `plain_chars` bytes hold no
    /// escape, validating them as UTF-8 first.
    #[inline(never)]
    fn advance_ground_checked<P: Perform>(
        &mut self,
        performer: &mut P,
        bytes: &[u8],
        plain_chars: usize,
    ) -> usize {
        let num_bytes = bytes.len();

        match str::from_utf8(&bytes[..plain_chars]) {
            Ok(parsed) => {
                Self::ground_dispatch(performer, parsed);
                let mut processed = plain_chars;

                // If there's another character, it must be escape so process it directly.
                if processed < num_bytes {
                    self.state = State::Escape;
                    self.reset_params();
                    processed += 1;
                }

                processed
            },
            // Handle invalid and partial utf8.
            Err(err) => {
                // Dispatch all the valid bytes.
                let valid_bytes = err.valid_up_to();
                let parsed = unsafe { str::from_utf8_unchecked(&bytes[..valid_bytes]) };
                Self::ground_dispatch(performer, parsed);

                match err.error_len() {
                    Some(len) => {
                        // Execute C1 escapes or emit replacement character.
                        if len == 1 && bytes[valid_bytes] <= 0x9F {
                            performer.execute(bytes[valid_bytes]);
                        } else {
                            performer.print('�');
                        }

                        // Restart processing after the invalid bytes.
                        //
                        // While we could theoretically try to just re-parse
                        // `bytes[valid_bytes + len..plain_chars]`, it's easier
                        // to just skip it and invalid utf8 is pretty rare anyway.
                        valid_bytes + len
                    },
                    None => {
                        if plain_chars < num_bytes {
                            // Process bytes cut off by escape.
                            performer.print('�');
                            self.state = State::Escape;
                            self.reset_params();
                            plain_chars + 1
                        } else {
                            // Process bytes cut off by the buffer end.
                            let extra_bytes = num_bytes - valid_bytes;
                            let partial_len = self.partial_utf8_len + extra_bytes;
                            self.partial_utf8[self.partial_utf8_len..partial_len]
                                .copy_from_slice(&bytes[valid_bytes..valid_bytes + extra_bytes]);
                            self.partial_utf8_len = partial_len;
                            num_bytes
                        }
                    },
                }
            },
        }
    }

    /// Advance the parser while processing a partial utf8 codepoint.
    #[inline]
    fn advance_partial_utf8<P: Perform>(&mut self, performer: &mut P, bytes: &[u8]) -> usize {
        // Try to copy up to 3 more characters, to ensure the codepoint is complete.
        let old_bytes = self.partial_utf8_len;
        let to_copy = bytes.len().min(self.partial_utf8.len() - old_bytes);
        self.partial_utf8[old_bytes..old_bytes + to_copy].copy_from_slice(&bytes[..to_copy]);
        self.partial_utf8_len += to_copy;

        // Parse the unicode character.
        match str::from_utf8(&self.partial_utf8[..self.partial_utf8_len]) {
            // If the entire buffer is valid, use the first character and continue parsing.
            Ok(parsed) => {
                let c = unsafe { parsed.chars().next().unwrap_unchecked() };
                performer.print(c);

                self.partial_utf8_len = 0;
                c.len_utf8() - old_bytes
            },
            Err(err) => {
                let valid_bytes = err.valid_up_to();
                // If we have any valid bytes, that means we partially copied another
                // utf8 character into `partial_utf8`. Since we only care about the
                // first character, we just ignore the rest. Ignoring means leaving it in
                // the input: only the first character's bytes count as consumed, or the
                // bytes between it and the cut character after it would never be parsed.
                if valid_bytes > 0 {
                    let c = unsafe {
                        let parsed = str::from_utf8_unchecked(&self.partial_utf8[..valid_bytes]);
                        parsed.chars().next().unwrap_unchecked()
                    };

                    performer.print(c);

                    self.partial_utf8_len = 0;
                    return c.len_utf8() - old_bytes;
                }

                match err.error_len() {
                    // If the partial character was also invalid, emit the replacement
                    // character.
                    Some(invalid_len) => {
                        performer.print('�');

                        self.partial_utf8_len = 0;
                        invalid_len - old_bytes
                    },
                    // If the character still isn't complete, wait for more data.
                    None => to_copy,
                }
            },
        }
    }

    /// Dispatch the printable text and the controls at the start of `bytes`, and return how many
    /// bytes that took: all of them unless an escape comes first (it is left, at the returned
    /// index), or a byte does not begin a well-formed UTF-8 character, or the character is cut
    /// off at the end.
    ///
    /// What the performer is told is what [`ground_dispatch`](Self::ground_dispatch) would
    /// tell it about the same bytes once they were checked.
    #[inline(always)]
    fn dispatch_text<P: Perform>(performer: &mut P, bytes: &[u8]) -> usize {
        let len = bytes.len();
        let mut i = 0;

        while i < len {
            // The longest run of printable characters: ASCII ones and valid others, but not
            // the C1 controls.
            let start = i;
            let mut ascii = true;
            while i < len {
                let byte = bytes[i];
                if (0x20..=0x7e).contains(&byte) {
                    i += 1;
                } else if byte < 0x80 {
                    break;
                } else {
                    match utf8_char_len(&bytes[i..]) {
                        Some(char_len) if !is_c1(&bytes[i..]) => {
                            i += char_len;
                            ascii = false;
                        },
                        _ => break,
                    }
                }
            }

            if i > start {
                // SAFETY: the bytes are ASCII, or characters `utf8_char_len` found well formed.
                let run = unsafe { str::from_utf8_unchecked(&bytes[start..i]) };
                if ascii {
                    performer.print_ascii(run);
                } else {
                    performer.print_text(run);
                }
            }

            if i == len {
                break;
            }

            // What ended the run: a control, DEL, or a character that is not text.
            let byte = bytes[i];
            if byte == 0x1B {
                return i;
            } else if byte < 0x20 {
                performer.execute(byte);
                i += 1;
            } else if byte == 0x7f {
                performer.print('\x7f');
                i += 1;
            } else if is_c1(&bytes[i..]) {
                performer.execute(bytes[i + 1]);
                i += 2;
            } else {
                return i;
            }
        }

        len
    }

    /// Handle ground dispatch of print/execute for all characters in a string.
    #[inline]
    fn ground_dispatch<P: Perform>(performer: &mut P, text: &str) {
        let bytes = text.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            // A run of printable ASCII goes to the performer as one string: per-character
            // work (a width lookup, a state check) is then paid once per run.
            let start = i;
            while i < bytes.len() && (0x20..=0x7e).contains(&bytes[i]) {
                i += 1;
            }
            if i > start {
                performer.print_ascii(&text[start..i]);
            }

            // Everything else, DEL included, takes the path it always took. `i` is on a
            // character boundary: the run before it is ASCII.
            if let Some(c) = text[i..].chars().next() {
                match c {
                    '\x00'..='\x1f' | '\u{80}'..='\u{9f}' => performer.execute(c as u8),
                    _ => performer.print(c),
                }
                i += c.len_utf8();
            }
        }
    }
}

/// The length of the well-formed UTF-8 character `bytes` starts with, whose first byte is not
/// ASCII; `None` if it is not well formed or the bytes end before it does.
#[inline(always)]
fn utf8_char_len(bytes: &[u8]) -> Option<usize> {
    let follows =
        |at: usize, low: u8, high: u8| matches!(bytes.get(at), Some(b) if (low..=high).contains(b));

    match bytes[0] {
        0xC2..=0xDF => follows(1, 0x80, 0xBF).then_some(2),
        0xE0 => (follows(1, 0xA0, 0xBF) && follows(2, 0x80, 0xBF)).then_some(3),
        0xE1..=0xEC | 0xEE..=0xEF => (follows(1, 0x80, 0xBF) && follows(2, 0x80, 0xBF)).then_some(3),
        0xED => (follows(1, 0x80, 0x9F) && follows(2, 0x80, 0xBF)).then_some(3),
        0xF0 => {
            (follows(1, 0x90, 0xBF) && follows(2, 0x80, 0xBF) && follows(3, 0x80, 0xBF))
                .then_some(4)
        },
        0xF1..=0xF3 => {
            (follows(1, 0x80, 0xBF) && follows(2, 0x80, 0xBF) && follows(3, 0x80, 0xBF))
                .then_some(4)
        },
        0xF4 => {
            (follows(1, 0x80, 0x8F) && follows(2, 0x80, 0xBF) && follows(3, 0x80, 0xBF))
                .then_some(4)
        },
        _ => None,
    }
}

/// Whether `bytes` starts with a C1 control, U+0080 to U+009F, in UTF-8.
#[inline(always)]
fn is_c1(bytes: &[u8]) -> bool {
    matches!(bytes, [0xC2, 0x80..=0x9F, ..])
}

#[derive(PartialEq, Eq, Debug, Default, Copy, Clone)]
enum State {
    CsiEntry,
    CsiIgnore,
    CsiIntermediate,
    CsiParam,
    DcsEntry,
    DcsIgnore,
    DcsIntermediate,
    DcsParam,
    DcsPassthrough,
    Escape,
    EscapeIntermediate,
    OscString,
    SosPmApcString,
    ApcString,
    ApcEscape,
    #[default]
    Ground,
}

/// Performs actions requested by the Parser
///
/// Actions in this case mean, for example, handling a CSI escape sequence
/// describing cursor movement, or simply printing characters to the screen.
///
/// The methods on this type correspond to actions described in
/// <http://vt100.net/emu/dec_ansi_parser>. I've done my best to describe them in
/// a useful way in my own words for completeness, but the site should be
/// referenced if something isn't clear. If the site disappears at some point in
/// the future, consider checking archive.org.
pub trait Perform {
    /// Draw a character to the screen and update states.
    fn print(&mut self, _c: char) {}

    /// Draw a run of printable ASCII (`0x20..=0x7e`, never empty) to the screen.
    ///
    /// The parser hands every maximal run of such bytes it finds in one read here instead
    /// of calling [`print`](Self::print) for each. The default does exactly that, so a
    /// performer that does not implement this behaves as before; one that can draw a run
    /// faster than its characters one by one overrides it and must leave the same state.
    /// A run ends where the read ends, so the same stream cut differently arrives as
    /// different runs.
    fn print_ascii(&mut self, s: &str) {
        for c in s.chars() {
            self.print(c);
        }
    }

    /// Draw a run of printable text to the screen: characters that are not controls, at least
    /// one of them outside ASCII.
    ///
    /// Like [`print_ascii`](Self::print_ascii), this is the parser handing over what it found
    /// together; the default calls [`print`](Self::print) for each character, and a performer
    /// that overrides it must leave the state that would leave.
    fn print_text(&mut self, s: &str) {
        for c in s.chars() {
            self.print(c);
        }
    }

    /// Execute a C0 or C1 control function.
    fn execute(&mut self, _byte: u8) {}

    /// Invoked when a final character arrives in first part of device control
    /// string.
    ///
    /// The control function should be determined from the private marker, final
    /// character, and execute with a parameter list. A handler should be
    /// selected for remaining characters in the string; the handler
    /// function should subsequently be called by `put` for every character in
    /// the control string.
    ///
    /// The `ignore` flag indicates that more than two intermediates arrived and
    /// subsequent characters were ignored.
    fn hook(&mut self, _params: &Params, _intermediates: &[u8], _ignore: bool, _action: char) {}

    /// Pass bytes as part of a device control string to the handle chosen in
    /// `hook`. C0 controls will also be passed to the handler.
    fn put(&mut self, _byte: u8) {}

    /// Called when a device control string is terminated.
    ///
    /// The previously selected handler should be notified that the DCS has
    /// terminated.
    fn unhook(&mut self) {}

    /// An application program command (`ESC _ … ESC \`) begins.
    ///
    /// Its body follows in [`Self::apc_put`], its end in [`Self::apc_end`]. The two other
    /// string types that look the same (`ESC X` and `ESC ^`) are dropped without a call.
    fn apc_start(&mut self) {}

    /// A piece of the body of the command begun by [`Self::apc_start`].
    ///
    /// The pieces are never empty and as long as the reads that carried them allow: the
    /// parser holds nothing back, so a command has as many pieces as reads cut it, and
    /// bounding its size is the performer's business. The bytes are not interpreted:
    /// controls, `DEL` and bytes above `0x7F` come as they are, only `ESC`, `CAN` and
    /// `SUB` end the body.
    fn apc_put(&mut self, _data: &[u8]) {}

    /// The command begun by [`Self::apc_start`] ended.
    ///
    /// `cancelled` tells how: `false` for `ESC \` (the command is complete), `true` for `CAN`,
    /// `SUB` or an `ESC` followed by any other byte, which the terminal must treat as if
    /// the command had never been sent.
    fn apc_end(&mut self, _cancelled: bool) {}

    /// Dispatch an operating system command.
    fn osc_dispatch(&mut self, _params: &[&[u8]], _bell_terminated: bool) {}

    /// A final character has arrived for a CSI sequence
    ///
    /// The `ignore` flag indicates that either more than two intermediates
    /// arrived or the number of parameters exceeded the maximum supported
    /// length, and subsequent characters were ignored.
    fn csi_dispatch(
        &mut self,
        _params: &Params,
        _intermediates: &[u8],
        _ignore: bool,
        _action: char,
    ) {
    }

    /// The final character of an escape sequence has arrived.
    ///
    /// The `ignore` flag indicates that more than two intermediates arrived and
    /// subsequent characters were ignored.
    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, _byte: u8) {}

    /// Whether the parser should terminate prematurely.
    ///
    /// This can be used in conjunction with
    /// [`Parser::advance_until_terminated`] to terminate the parser after
    /// receiving certain escape sequences like synchronized updates.
    ///
    /// This is checked after every parsed byte, so no expensive computation
    /// should take place in this function.
    #[inline(always)]
    fn terminated(&self) -> bool {
        false
    }
}

#[cfg(all(test, not(feature = "std")))]
#[macro_use]
extern crate std;

#[cfg(test)]
mod tests {
    use std::vec::Vec;

    use super::*;

    const OSC_BYTES: &[u8] = &[
        0x1B, 0x5D, // Begin OSC
        b'2', b';', b'j', b'w', b'i', b'l', b'm', b'@', b'j', b'w', b'i', b'l', b'm', b'-', b'd',
        b'e', b's', b'k', b':', b' ', b'~', b'/', b'c', b'o', b'd', b'e', b'/', b'a', b'l', b'a',
        b'c', b'r', b'i', b't', b't', b'y', 0x07, // End OSC
    ];

    #[derive(Default)]
    struct Dispatcher {
        dispatched: Vec<Sequence>,
        /// The runs of printable ASCII, as `print_ascii` got them.
        runs: Vec<String>,
        /// The runs of other printable text, as `print_text` got them.
        texts: Vec<String>,
    }

    #[derive(Debug, PartialEq, Eq)]
    enum Sequence {
        Osc(Vec<Vec<u8>>, bool),
        Csi(Vec<Vec<u16>>, Vec<u8>, bool, char),
        Esc(Vec<u8>, bool, u8),
        DcsHook(Vec<Vec<u16>>, Vec<u8>, bool, char),
        DcsPut(u8),
        Print(char),
        Execute(u8),
        DcsUnhook,
        ApcStart,
        ApcPut(Vec<u8>),
        ApcEnd(bool),
    }

    impl Perform for Dispatcher {
        fn osc_dispatch(&mut self, params: &[&[u8]], bell_terminated: bool) {
            let params = params.iter().map(|p| p.to_vec()).collect();
            self.dispatched.push(Sequence::Osc(params, bell_terminated));
        }

        fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, c: char) {
            let params = params.iter().map(|subparam| subparam.to_vec()).collect();
            let intermediates = intermediates.to_vec();
            self.dispatched.push(Sequence::Csi(params, intermediates, ignore, c));
        }

        fn esc_dispatch(&mut self, intermediates: &[u8], ignore: bool, byte: u8) {
            let intermediates = intermediates.to_vec();
            self.dispatched.push(Sequence::Esc(intermediates, ignore, byte));
        }

        fn hook(&mut self, params: &Params, intermediates: &[u8], ignore: bool, c: char) {
            let params = params.iter().map(|subparam| subparam.to_vec()).collect();
            let intermediates = intermediates.to_vec();
            self.dispatched.push(Sequence::DcsHook(params, intermediates, ignore, c));
        }

        fn put(&mut self, byte: u8) {
            self.dispatched.push(Sequence::DcsPut(byte));
        }

        fn unhook(&mut self) {
            self.dispatched.push(Sequence::DcsUnhook);
        }

        fn print(&mut self, c: char) {
            self.dispatched.push(Sequence::Print(c));
        }

        /// Takes the runs whole, and tells the same as `print` per character would, so every
        /// test below that compares what was dispatched also holds the runs to it.
        fn print_ascii(&mut self, s: &str) {
            assert!(!s.is_empty());
            assert!(s.bytes().all(|byte| (0x20..=0x7e).contains(&byte)), "{s:?}");
            self.runs.push(s.to_owned());
            for c in s.chars() {
                self.dispatched.push(Sequence::Print(c));
            }
        }

        fn execute(&mut self, byte: u8) {
            self.dispatched.push(Sequence::Execute(byte));
        }

        /// The same for text: it is no control, and some of it is not ASCII.
        fn print_text(&mut self, s: &str) {
            assert!(!s.is_ascii(), "{s:?}");
            assert!(s.chars().all(|c| !matches!(c, '\x00'..='\x1f' | '\x7f'..='\u{9f}')), "{s:?}");
            self.texts.push(s.to_owned());
            for c in s.chars() {
                self.dispatched.push(Sequence::Print(c));
            }
        }

        fn apc_start(&mut self) {
            self.dispatched.push(Sequence::ApcStart);
        }

        fn apc_put(&mut self, data: &[u8]) {
            assert!(!data.is_empty());
            self.dispatched.push(Sequence::ApcPut(data.to_vec()));
        }

        fn apc_end(&mut self, cancelled: bool) {
            self.dispatched.push(Sequence::ApcEnd(cancelled));
        }
    }

    #[test]
    fn parse_osc() {
        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, OSC_BYTES);

        assert_eq!(dispatcher.dispatched.len(), 1);
        match &dispatcher.dispatched[0] {
            Sequence::Osc(params, _) => {
                assert_eq!(params.len(), 2);
                assert_eq!(params[0], &OSC_BYTES[2..3]);
                assert_eq!(params[1], &OSC_BYTES[4..(OSC_BYTES.len() - 1)]);
            },
            _ => panic!("expected osc sequence"),
        }
    }

    #[test]
    fn parse_empty_osc() {
        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, &[0x1B, 0x5D, 0x07]);

        assert_eq!(dispatcher.dispatched.len(), 1);
        match &dispatcher.dispatched[0] {
            Sequence::Osc(..) => (),
            _ => panic!("expected osc sequence"),
        }
    }

    #[test]
    fn parse_osc_max_params() {
        let params = ";".repeat(params::MAX_PARAMS + 1);
        let input = format!("\x1b]{}\x1b", &params[..]).into_bytes();
        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, &input);

        assert_eq!(dispatcher.dispatched.len(), 1);
        match &dispatcher.dispatched[0] {
            Sequence::Osc(params, _) => {
                assert_eq!(params.len(), MAX_OSC_PARAMS);
                assert!(params.iter().all(Vec::is_empty));
            },
            _ => panic!("expected osc sequence"),
        }
    }

    #[test]
    fn osc_bell_terminated() {
        const INPUT: &[u8] = b"\x1b]11;ff/00/ff\x07";
        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, INPUT);

        assert_eq!(dispatcher.dispatched.len(), 1);
        match &dispatcher.dispatched[0] {
            Sequence::Osc(_, true) => (),
            _ => panic!("expected osc with bell terminator"),
        }
    }

    #[test]
    fn osc_c0_st_terminated() {
        const INPUT: &[u8] = b"\x1b]11;ff/00/ff\x1b\\";
        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, INPUT);

        assert_eq!(dispatcher.dispatched.len(), 2);
        match &dispatcher.dispatched[0] {
            Sequence::Osc(_, false) => (),
            _ => panic!("expected osc with ST terminator"),
        }
    }

    #[test]
    fn parse_osc_with_utf8_arguments() {
        const INPUT: &[u8] = &[
            0x0D, 0x1B, 0x5D, 0x32, 0x3B, 0x65, 0x63, 0x68, 0x6F, 0x20, 0x27, 0xC2, 0xAF, 0x5C,
            0x5F, 0x28, 0xE3, 0x83, 0x84, 0x29, 0x5F, 0x2F, 0xC2, 0xAF, 0x27, 0x20, 0x26, 0x26,
            0x20, 0x73, 0x6C, 0x65, 0x65, 0x70, 0x20, 0x31, 0x07,
        ];
        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, INPUT);

        assert_eq!(dispatcher.dispatched[0], Sequence::Execute(b'\r'));
        let osc_data = INPUT[5..(INPUT.len() - 1)].into();
        assert_eq!(dispatcher.dispatched[1], Sequence::Osc(vec![vec![b'2'], osc_data], true));
        assert_eq!(dispatcher.dispatched.len(), 2);
    }

    #[test]
    fn osc_containing_string_terminator() {
        const INPUT: &[u8] = b"\x1b]2;\xe6\x9c\xab\x1b\\";
        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, INPUT);

        assert_eq!(dispatcher.dispatched.len(), 2);
        match &dispatcher.dispatched[0] {
            Sequence::Osc(params, _) => {
                assert_eq!(params[1], &INPUT[4..(INPUT.len() - 2)]);
            },
            _ => panic!("expected osc sequence"),
        }
    }

    #[test]
    fn exceed_max_buffer_size() {
        const NUM_BYTES: usize = MAX_OSC_RAW + 100;
        const INPUT_START: &[u8] = b"\x1b]52;s";
        const INPUT_END: &[u8] = b"\x07";

        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        // Create valid OSC escape
        parser.advance(&mut dispatcher, INPUT_START);

        // Exceed max buffer size
        parser.advance(&mut dispatcher, &[b'a'; NUM_BYTES]);

        // Terminate escape for dispatch
        parser.advance(&mut dispatcher, INPUT_END);

        assert_eq!(dispatcher.dispatched.len(), 1);
        match &dispatcher.dispatched[0] {
            Sequence::Osc(params, _) => {
                assert_eq!(params.len(), 2);
                assert_eq!(params[0], b"52");

                #[cfg(feature = "std")]
                assert_eq!(params[1].len(), NUM_BYTES + INPUT_END.len());

                #[cfg(not(feature = "std"))]
                assert_eq!(params[1].len(), MAX_OSC_RAW - params[0].len());
            },
            _ => panic!("expected osc sequence"),
        }
    }

    #[test]
    fn parse_csi_max_params() {
        // This will build a list of repeating '1;'s
        // The length is MAX_PARAMS - 1 because the last semicolon is interpreted
        // as an implicit zero, making the total number of parameters MAX_PARAMS
        let params = "1;".repeat(params::MAX_PARAMS - 1);
        let input = format!("\x1b[{}p", &params[..]).into_bytes();

        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, &input);

        assert_eq!(dispatcher.dispatched.len(), 1);
        match &dispatcher.dispatched[0] {
            Sequence::Csi(params, _, ignore, _) => {
                assert_eq!(params.len(), params::MAX_PARAMS);
                assert!(!ignore);
            },
            _ => panic!("expected csi sequence"),
        }
    }

    #[test]
    fn parse_csi_params_ignore_long_params() {
        // This will build a list of repeating '1;'s
        // The length is MAX_PARAMS because the last semicolon is interpreted
        // as an implicit zero, making the total number of parameters MAX_PARAMS + 1
        let params = "1;".repeat(params::MAX_PARAMS);
        let input = format!("\x1b[{}p", &params[..]).into_bytes();

        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, &input);

        assert_eq!(dispatcher.dispatched.len(), 1);
        match &dispatcher.dispatched[0] {
            Sequence::Csi(params, _, ignore, _) => {
                assert_eq!(params.len(), params::MAX_PARAMS);
                assert!(ignore);
            },
            _ => panic!("expected csi sequence"),
        }
    }

    #[test]
    fn parse_csi_params_trailing_semicolon() {
        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, b"\x1b[4;m");

        assert_eq!(dispatcher.dispatched.len(), 1);
        match &dispatcher.dispatched[0] {
            Sequence::Csi(params, ..) => assert_eq!(params, &[[4], [0]]),
            _ => panic!("expected csi sequence"),
        }
    }

    #[test]
    fn parse_csi_params_leading_semicolon() {
        // Create dispatcher and check state
        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, b"\x1b[;4m");

        assert_eq!(dispatcher.dispatched.len(), 1);
        match &dispatcher.dispatched[0] {
            Sequence::Csi(params, ..) => assert_eq!(params, &[[0], [4]]),
            _ => panic!("expected csi sequence"),
        }
    }

    #[test]
    fn parse_long_csi_param() {
        // The important part is the parameter, which is (i64::MAX + 1)
        const INPUT: &[u8] = b"\x1b[9223372036854775808m";
        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, INPUT);

        assert_eq!(dispatcher.dispatched.len(), 1);
        match &dispatcher.dispatched[0] {
            Sequence::Csi(params, ..) => assert_eq!(params, &[[u16::MAX]]),
            _ => panic!("expected csi sequence"),
        }
    }

    #[test]
    fn csi_reset() {
        const INPUT: &[u8] = b"\x1b[3;1\x1b[?1049h";
        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, INPUT);

        assert_eq!(dispatcher.dispatched.len(), 1);
        match &dispatcher.dispatched[0] {
            Sequence::Csi(params, intermediates, ignore, _) => {
                assert_eq!(intermediates, b"?");
                assert_eq!(params, &[[1049]]);
                assert!(!ignore);
            },
            _ => panic!("expected csi sequence"),
        }
    }

    #[test]
    fn csi_subparameters() {
        const INPUT: &[u8] = b"\x1b[38:2:255:0:255;1m";
        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, INPUT);

        assert_eq!(dispatcher.dispatched.len(), 1);
        match &dispatcher.dispatched[0] {
            Sequence::Csi(params, intermediates, ignore, _) => {
                assert_eq!(params, &[vec![38, 2, 255, 0, 255], vec![1]]);
                assert_eq!(intermediates, &[]);
                assert!(!ignore);
            },
            _ => panic!("expected csi sequence"),
        }
    }

    #[test]
    fn parse_dcs_max_params() {
        let params = "1;".repeat(params::MAX_PARAMS + 1);
        let input = format!("\x1bP{}p", &params[..]).into_bytes();
        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, &input);

        assert_eq!(dispatcher.dispatched.len(), 1);
        match &dispatcher.dispatched[0] {
            Sequence::DcsHook(params, _, ignore, _) => {
                assert_eq!(params.len(), params::MAX_PARAMS);
                assert!(params.iter().all(|param| param == &[1]));
                assert!(ignore);
            },
            _ => panic!("expected dcs sequence"),
        }
    }

    #[test]
    fn dcs_reset() {
        const INPUT: &[u8] = b"\x1b[3;1\x1bP1$tx\x9c";
        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, INPUT);

        assert_eq!(dispatcher.dispatched.len(), 3);
        match &dispatcher.dispatched[0] {
            Sequence::DcsHook(params, intermediates, ignore, _) => {
                assert_eq!(intermediates, b"$");
                assert_eq!(params, &[[1]]);
                assert!(!ignore);
            },
            _ => panic!("expected dcs sequence"),
        }
        assert_eq!(dispatcher.dispatched[1], Sequence::DcsPut(b'x'));
        assert_eq!(dispatcher.dispatched[2], Sequence::DcsUnhook);
    }

    #[test]
    fn parse_dcs() {
        const INPUT: &[u8] = b"\x1bP0;1|17/ab\x9c";
        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, INPUT);

        assert_eq!(dispatcher.dispatched.len(), 7);
        match &dispatcher.dispatched[0] {
            Sequence::DcsHook(params, _, _, c) => {
                assert_eq!(params, &[[0], [1]]);
                assert_eq!(c, &'|');
            },
            _ => panic!("expected dcs sequence"),
        }
        for (i, byte) in b"17/ab".iter().enumerate() {
            assert_eq!(dispatcher.dispatched[1 + i], Sequence::DcsPut(*byte));
        }
        assert_eq!(dispatcher.dispatched[6], Sequence::DcsUnhook);
    }

    #[test]
    fn intermediate_reset_on_dcs_exit() {
        const INPUT: &[u8] = b"\x1bP=1sZZZ\x1b+\x5c";
        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, INPUT);

        assert_eq!(dispatcher.dispatched.len(), 6);
        match &dispatcher.dispatched[5] {
            Sequence::Esc(intermediates, ..) => assert_eq!(intermediates, b"+"),
            _ => panic!("expected esc sequence"),
        }
    }

    #[test]
    fn esc_reset() {
        const INPUT: &[u8] = b"\x1b[3;1\x1b(A";
        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, INPUT);

        assert_eq!(dispatcher.dispatched.len(), 1);
        match &dispatcher.dispatched[0] {
            Sequence::Esc(intermediates, ignore, byte) => {
                assert_eq!(intermediates, b"(");
                assert_eq!(*byte, b'A');
                assert!(!ignore);
            },
            _ => panic!("expected esc sequence"),
        }
    }

    #[test]
    fn esc_reset_intermediates() {
        const INPUT: &[u8] = b"\x1b[?2004l\x1b#8";
        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, INPUT);

        assert_eq!(dispatcher.dispatched.len(), 2);
        assert_eq!(dispatcher.dispatched[0], Sequence::Csi(vec![vec![2004]], vec![63], false, 'l'));
        assert_eq!(dispatcher.dispatched[1], Sequence::Esc(vec![35], false, 56));
    }

    #[test]
    fn params_buffer_filled_with_subparam() {
        const INPUT: &[u8] = b"\x1b[::::::::::::::::::::::::::::::::x\x1b";
        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, INPUT);

        assert_eq!(dispatcher.dispatched.len(), 1);
        match &dispatcher.dispatched[0] {
            Sequence::Csi(params, intermediates, ignore, c) => {
                assert_eq!(intermediates, &[]);
                assert_eq!(params, &[[0; 32]]);
                assert_eq!(c, &'x');
                assert!(ignore);
            },
            _ => panic!("expected csi sequence"),
        }
    }

    #[cfg(not(feature = "std"))]
    #[test]
    fn build_with_fixed_size() {
        const INPUT: &[u8] = b"\x1b[3;1\x1b[?1049h";
        let mut dispatcher = Dispatcher::default();
        let mut parser: Parser<30> = Parser::new_with_size();

        parser.advance(&mut dispatcher, INPUT);

        assert_eq!(dispatcher.dispatched.len(), 1);
        match &dispatcher.dispatched[0] {
            Sequence::Csi(params, intermediates, ignore, _) => {
                assert_eq!(intermediates, b"?");
                assert_eq!(params, &[[1049]]);
                assert!(!ignore);
            },
            _ => panic!("expected csi sequence"),
        }
    }

    #[cfg(not(feature = "std"))]
    #[test]
    fn exceed_fixed_osc_buffer_size() {
        const OSC_BUFFER_SIZE: usize = 32;
        const NUM_BYTES: usize = OSC_BUFFER_SIZE + 100;
        const INPUT_START: &[u8] = b"\x1b]52;";
        const INPUT_END: &[u8] = b"\x07";

        let mut dispatcher = Dispatcher::default();
        let mut parser: Parser<OSC_BUFFER_SIZE> = Parser::new_with_size();

        // Create valid OSC escape
        parser.advance(&mut dispatcher, INPUT_START);

        // Exceed max buffer size
        parser.advance(&mut dispatcher, &[b'a'; NUM_BYTES]);

        // Terminate escape for dispatch
        parser.advance(&mut dispatcher, INPUT_END);

        assert_eq!(dispatcher.dispatched.len(), 1);
        match &dispatcher.dispatched[0] {
            Sequence::Osc(params, _) => {
                assert_eq!(params.len(), 2);
                assert_eq!(params[0], b"52");
                assert_eq!(params[1].len(), OSC_BUFFER_SIZE - params[0].len());
                for item in params[1].iter() {
                    assert_eq!(*item, b'a');
                }
            },
            _ => panic!("expected osc sequence"),
        }
    }

    #[cfg(not(feature = "std"))]
    #[test]
    fn fixed_size_osc_containing_string_terminator() {
        const INPUT_START: &[u8] = b"\x1b]2;";
        const INPUT_MIDDLE: &[u8] = b"s\xe6\x9c\xab";
        const INPUT_END: &[u8] = b"\x1b\\";

        let mut dispatcher = Dispatcher::default();
        let mut parser: Parser<5> = Parser::new_with_size();

        parser.advance(&mut dispatcher, INPUT_START);
        parser.advance(&mut dispatcher, INPUT_MIDDLE);
        parser.advance(&mut dispatcher, INPUT_END);

        assert_eq!(dispatcher.dispatched.len(), 2);
        match &dispatcher.dispatched[0] {
            Sequence::Osc(params, false) => {
                assert_eq!(params[0], b"2");
                assert_eq!(params[1], INPUT_MIDDLE);
            },
            _ => panic!("expected osc sequence"),
        }
    }

    #[test]
    fn unicode() {
        const INPUT: &[u8] = b"\xF0\x9F\x8E\x89_\xF0\x9F\xA6\x80\xF0\x9F\xA6\x80_\xF0\x9F\x8E\x89";

        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, INPUT);

        assert_eq!(dispatcher.dispatched.len(), 6);
        assert_eq!(dispatcher.dispatched[0], Sequence::Print('🎉'));
        assert_eq!(dispatcher.dispatched[1], Sequence::Print('_'));
        assert_eq!(dispatcher.dispatched[2], Sequence::Print('🦀'));
        assert_eq!(dispatcher.dispatched[3], Sequence::Print('🦀'));
        assert_eq!(dispatcher.dispatched[4], Sequence::Print('_'));
        assert_eq!(dispatcher.dispatched[5], Sequence::Print('🎉'));
    }

    #[test]
    fn invalid_utf8() {
        const INPUT: &[u8] = b"a\xEF\xBCb";

        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, INPUT);

        assert_eq!(dispatcher.dispatched.len(), 3);
        assert_eq!(dispatcher.dispatched[0], Sequence::Print('a'));
        assert_eq!(dispatcher.dispatched[1], Sequence::Print('�'));
        assert_eq!(dispatcher.dispatched[2], Sequence::Print('b'));
    }

    #[test]
    fn partial_utf8() {
        const INPUT: &[u8] = b"\xF0\x9F\x9A\x80";

        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, &INPUT[..1]);
        parser.advance(&mut dispatcher, &INPUT[1..2]);
        parser.advance(&mut dispatcher, &INPUT[2..3]);
        parser.advance(&mut dispatcher, &INPUT[3..]);

        assert_eq!(dispatcher.dispatched.len(), 1);
        assert_eq!(dispatcher.dispatched[0], Sequence::Print('🚀'));
    }

    #[test]
    fn partial_utf8_separating_utf8() {
        // This is different from the `partial_utf8` test since it has a multi-byte UTF8
        // character after the partial UTF8 state, causing a partial byte to be present
        // in the `partial_utf8` buffer after the 2-byte codepoint.

        // "ĸ🎉"
        const INPUT: &[u8] = b"\xC4\xB8\xF0\x9F\x8E\x89";

        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, &INPUT[..1]);
        parser.advance(&mut dispatcher, &INPUT[1..]);

        assert_eq!(dispatcher.dispatched.len(), 2);
        assert_eq!(dispatcher.dispatched[0], Sequence::Print('ĸ'));
        assert_eq!(dispatcher.dispatched[1], Sequence::Print('🎉'));
    }

    #[test]
    fn partial_utf8_keeps_the_bytes_after_the_character() {
        // "· ·": the first read ends inside the first `·`. The three bytes copied to
        // complete it are the rest of that `·`, a space, and the start of the next `·`: the
        // space must survive.
        const INPUT: &[u8] = b"\xC2\xB7 \xC2\xB7";

        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, &INPUT[..1]);
        parser.advance(&mut dispatcher, &INPUT[1..]);

        assert_eq!(dispatcher.dispatched, [
            Sequence::Print('·'),
            Sequence::Print(' '),
            Sequence::Print('·'),
        ]);
    }

    /// `input` through one parser in the reads `cuts` marks, the way a terminal gets a stream.
    fn dispatcher_after_reads(input: &[u8], cuts: &[usize]) -> Dispatcher {
        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        let mut start = 0;
        for end in cuts.iter().copied().chain(core::iter::once(input.len())) {
            parser.advance(&mut dispatcher, &input[start..end]);
            start = end;
        }

        dispatcher
    }

    fn dispatched_in_reads(input: &[u8], cuts: &[usize]) -> Vec<Sequence> {
        dispatcher_after_reads(input, cuts).dispatched
    }

    #[test]
    fn utf8_cut_anywhere_dispatches_what_the_whole_does() {
        // One-, two-, three- and four-byte characters next to each other, next to ASCII and
        // around an escape sequence, so a cut lands inside a character whose neighbours are
        // themselves cut by the bytes read ahead to complete it.
        const INPUT: &str = "a· ·é€x😀 ·🎉€·é\x1b[1m·\x1b[0m ĸ🎉·\u{10FFFF}€ \u{7FF}·";

        let input = INPUT.as_bytes();
        let whole = dispatched_in_reads(input, &[]);
        let printed = whole.iter().filter(|sequence| matches!(sequence, Sequence::Print(_)));
        assert_eq!(printed.count(), INPUT.chars().count() - "\x1b[1m\x1b[0m".len());

        for first in 0..=input.len() {
            for second in first..=input.len() {
                assert_eq!(
                    dispatched_in_reads(input, &[first, second]),
                    whole,
                    "cut at {first} and {second}"
                );
            }
        }

        let every_byte: Vec<usize> = (1..input.len()).collect();
        assert_eq!(dispatched_in_reads(input, &every_byte), whole);
    }

    #[test]
    fn printable_ascii_arrives_in_maximal_runs() {
        // Everything else, DEL and the characters outside ASCII included, is printed or
        // executed one by one, as it always was, and ends the run before it.
        const INPUT: &[u8] = b"ab c\r\nd\x7fe \xC3\xA9 f\x1b[1mg\x85h~";

        let dispatcher = dispatcher_after_reads(INPUT, &[]);
        assert_eq!(dispatcher.runs, ["ab c", "d", "g", "h~"]);
        assert_eq!(dispatcher.texts, ["e \u{e9} f"]);
        assert_eq!(dispatcher.dispatched.iter().filter(|s| matches!(s, Sequence::Print(_))).count(), 14);
        assert!(dispatcher.dispatched.contains(&Sequence::Print('\x7f')));
        assert!(dispatcher.dispatched.contains(&Sequence::Print('é')));
        assert!(dispatcher.dispatched.contains(&Sequence::Execute(0x85)));

        // A read ends a run: what comes in the next one starts another.
        let dispatcher = dispatcher_after_reads(b"abcdef", &[2, 3]);
        assert_eq!(dispatcher.runs, ["ab", "c", "def"]);
    }

    #[test]
    fn printable_ascii_cut_anywhere_dispatches_what_the_whole_does() {
        // Runs next to controls, DEL, characters of two to four bytes, an escape sequence and
        // invalid bytes, so a cut lands inside a run, at its ends and inside the characters
        // around it. (No C1 control as UTF-8: a character cut inside it is printed, not
        // executed, which is how upstream's `advance_partial_utf8` always took it.)
        const INPUT: &[u8] =
            b"abc def\r\n~ \x7fgh \xC3\xA9ij\xF0\x9F\x8E\x89 k\x1b[1mlm\x1b[0m\xFFno\xE2\x82 pq\x08r";

        let whole = dispatched_in_reads(INPUT, &[]);
        for first in 0..=INPUT.len() {
            for second in first..=INPUT.len() {
                let cut = dispatcher_after_reads(INPUT, &[first, second]);
                assert_eq!(cut.dispatched, whole, "cut at {first} and {second}");
            }
        }

        let every_byte: Vec<usize> = (1..INPUT.len()).collect();
        assert_eq!(dispatched_in_reads(INPUT, &every_byte), whole);
    }

    #[test]
    fn printable_text_arrives_in_maximal_runs() {
        // Text outside ASCII comes with the ASCII around it, up to a control, DEL or a C1
        // control (U+0085 is the UTF-8 `C2 85`); a run of ASCII alone is the other kind.
        const INPUT: &str = "ab \u{e7}\u{131}\r\ncd\u{7f}\u{65e5}\u{672c} z\u{85}y\u{1f389}x";

        let dispatcher = dispatcher_after_reads(INPUT.as_bytes(), &[]);
        assert_eq!(dispatcher.runs, ["cd"]);
        assert_eq!(dispatcher.texts, ["ab \u{e7}\u{131}", "\u{65e5}\u{672c} z", "y\u{1f389}x"]);
        assert!(dispatcher.dispatched.contains(&Sequence::Execute(0x85)));
        assert!(dispatcher.dispatched.contains(&Sequence::Print('\u{7f}')));

        // A read ends a run, in the middle of a character as well.
        let dispatcher = dispatcher_after_reads("a\u{e7}b\u{65e5}c".as_bytes(), &[1, 4]);
        assert_eq!(dispatcher.runs, ["a"]);
        assert_eq!(dispatcher.texts, ["\u{e7}b", "\u{65e5}c"]);
    }

    /// The bytes of a read the way a program writes them: text, controls, DEL, C1 controls and
    /// bytes that are no valid UTF-8, in every way they can be cut off.
    fn byte_soup(seed: u64, c1: bool, esc: bool) -> Vec<u8> {
        let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        let mut next = move |bound: usize| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state % bound as u64) as usize
        };

        const PIECES: &[&[u8]] = &[
            b"a", b"hello", b" ", b"~", b"\x00", b"\r\n", b"\x08", b"\x7f", b"\x1f", b"\xC2\x85",
            b"\xC2\x9F", b"\xC2\xA0", b"\xC3\xA9", "\u{131}".as_bytes(), "\u{65e5}".as_bytes(),
            "\u{1f389}".as_bytes(), "\u{fffd}".as_bytes(), "\u{10ffff}".as_bytes(), b"\xC0\x80",
            b"\xC1\xBF", b"\xE0\x80\x80", b"\xE0\x9F\xBF", b"\xED\xA0\x80", b"\xED\x9F\xBF",
            b"\xF0\x80\x80\x80", b"\xF0\x8F\xBF\xBF", b"\xF4\x90\x80\x80", b"\xF4\x8F\xBF\xBF",
            b"\xF5", b"\xFF", b"\x80", b"\xBF", b"\xC3", b"\xE6\x97", b"\xF0\x9F\x8E",
            b"\xE6\x97\xA5\xE6", b"\xF0\x9F", b"\x1b[31m", b"\x1b[0m", b"\x1b", b"\x1b[",
            b"\x1b[1;38;5;9m", b"\x1b[?25l", b"\x1b]0;t\x07", b"\x1b(B",
        ];

        let mut bytes = Vec::new();
        for _ in 0..(1 + next(24)) {
            let piece = PIECES[next(PIECES.len())];
            let is_c1 = matches!(piece, [0xC2, 0x80..=0x9F]);
            if (c1 || !is_c1) && (esc || !piece.contains(&0x1B)) {
                bytes.extend_from_slice(piece);
            }
        }
        bytes
    }

    #[test]
    fn text_is_dispatched_as_checking_it_first_would() {
        // `advance_ground_checked` is the way text was parsed before it was dispatched while
        // being checked: validate with `str::from_utf8`, dispatch the valid part, deal with
        // what is not. The faster way must tell the performer, and leave the parser, as that
        // does.
        for seed in 1..=20_000 {
            let bytes = byte_soup(seed, true, false);

            let mut fast = Dispatcher::default();
            let mut fast_parser = Parser::new();
            fast_parser.advance(&mut fast, &bytes);

            let mut slow = Dispatcher::default();
            let mut slow_parser = Parser::new();
            let mut at = 0;
            while at < bytes.len() {
                at += slow_parser.advance_ground_checked(&mut slow, &bytes[at..], bytes.len() - at);
            }

            assert_eq!(fast.dispatched, slow.dispatched, "seed {seed}: {bytes:02x?}");
            assert_eq!(fast_parser.partial_utf8_len, slow_parser.partial_utf8_len, "seed {seed}");
            assert_eq!(
                fast_parser.partial_utf8[..fast_parser.partial_utf8_len],
                slow_parser.partial_utf8[..slow_parser.partial_utf8_len],
                "seed {seed}"
            );
        }
    }

    #[test]
    fn text_cut_anywhere_dispatches_what_the_whole_does() {
        // (Without C1 controls: a character cut inside one is printed, as it always was.)
        for seed in 1..=300 {
            let bytes = byte_soup(seed, false, true);
            let whole = dispatched_in_reads(&bytes, &[]);
            for first in 0..=bytes.len() {
                for second in first..=bytes.len() {
                    assert_eq!(
                        dispatched_in_reads(&bytes, &[first, second]),
                        whole,
                        "seed {seed}: {bytes:02x?} cut at {first} and {second}"
                    );
                }
            }
        }
    }

    #[test]
    fn well_formed_characters_are_what_std_calls_so() {
        // `utf8_char_len` must take exactly the characters `str::from_utf8` takes: every pair
        // of bytes, every triple that starts a three-byte character and the corners of the
        // four-byte ones.
        let std_len = |bytes: &[u8]| -> Option<usize> {
            let wanted = match bytes[0] {
                0xC2..=0xDF => 2,
                0xE0..=0xEF => 3,
                0xF0..=0xF4 => 4,
                _ => return None,
            };
            let taken = bytes.get(..wanted)?;
            str::from_utf8(taken).ok().map(|_| wanted)
        };

        for lead in 0x80..=0xFF_u8 {
            for second in 0..=0xFF_u8 {
                assert_eq!(utf8_char_len(&[lead, second]), std_len(&[lead, second]).filter(|&n| n == 2));
                for third in 0..=0xFF_u8 {
                    let bytes = [lead, second, third];
                    assert_eq!(utf8_char_len(&bytes), std_len(&bytes).filter(|&n| n <= 3), "{bytes:02x?}");
                }
            }
        }
        for lead in 0xF0..=0xF5_u8 {
            for second in [0x7F, 0x80, 0x8F, 0x90, 0xBF, 0xC0] {
                for third in [0x7F, 0x80, 0xBF, 0xC0] {
                    for fourth in [0x7F, 0x80, 0xBF, 0xC0] {
                        let bytes = [lead, second, third, fourth];
                        assert_eq!(utf8_char_len(&bytes), std_len(&bytes), "{bytes:02x?}");
                    }
                }
            }
        }
        // And the ends of a read.
        assert_eq!(utf8_char_len(&[0xE6]), None);
        assert_eq!(utf8_char_len(&[0xF0, 0x9F, 0x8E]), None);
    }

    #[test]
    fn partial_invalid_utf8() {
        const INPUT: &[u8] = b"a\xEF\xBCb";

        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, &INPUT[..1]);
        parser.advance(&mut dispatcher, &INPUT[1..2]);
        parser.advance(&mut dispatcher, &INPUT[2..3]);
        parser.advance(&mut dispatcher, &INPUT[3..]);

        assert_eq!(dispatcher.dispatched.len(), 3);
        assert_eq!(dispatcher.dispatched[0], Sequence::Print('a'));
        assert_eq!(dispatcher.dispatched[1], Sequence::Print('�'));
        assert_eq!(dispatcher.dispatched[2], Sequence::Print('b'));
    }

    #[test]
    fn partial_invalid_utf8_split() {
        const INPUT: &[u8] = b"\xE4\xBF\x99\xB5";

        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, &INPUT[..2]);
        parser.advance(&mut dispatcher, &INPUT[2..]);

        assert_eq!(dispatcher.dispatched[0], Sequence::Print('俙'));
        assert_eq!(dispatcher.dispatched[1], Sequence::Print('�'));
    }

    #[test]
    fn partial_utf8_into_esc() {
        const INPUT: &[u8] = b"\xD8\x1b012";

        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, INPUT);

        assert_eq!(dispatcher.dispatched.len(), 4);
        assert_eq!(dispatcher.dispatched[0], Sequence::Print('�'));
        assert_eq!(dispatcher.dispatched[1], Sequence::Esc(Vec::new(), false, b'0'));
        assert_eq!(dispatcher.dispatched[2], Sequence::Print('1'));
        assert_eq!(dispatcher.dispatched[3], Sequence::Print('2'));
    }

    #[test]
    fn c1s() {
        const INPUT: &[u8] = b"\x00\x1f\x80\x90\x98\x9b\x9c\x9d\x9e\x9fa";

        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, INPUT);

        assert_eq!(dispatcher.dispatched.len(), 11);
        assert_eq!(dispatcher.dispatched[0], Sequence::Execute(0));
        assert_eq!(dispatcher.dispatched[1], Sequence::Execute(31));
        assert_eq!(dispatcher.dispatched[2], Sequence::Execute(128));
        assert_eq!(dispatcher.dispatched[3], Sequence::Execute(144));
        assert_eq!(dispatcher.dispatched[4], Sequence::Execute(152));
        assert_eq!(dispatcher.dispatched[5], Sequence::Execute(155));
        assert_eq!(dispatcher.dispatched[6], Sequence::Execute(156));
        assert_eq!(dispatcher.dispatched[7], Sequence::Execute(157));
        assert_eq!(dispatcher.dispatched[8], Sequence::Execute(158));
        assert_eq!(dispatcher.dispatched[9], Sequence::Execute(159));
        assert_eq!(dispatcher.dispatched[10], Sequence::Print('a'));
    }

    #[test]
    fn execute_anywhere() {
        const INPUT: &[u8] = b"\x18\x1a";

        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, INPUT);

        assert_eq!(dispatcher.dispatched.len(), 2);
        assert_eq!(dispatcher.dispatched[0], Sequence::Execute(0x18));
        assert_eq!(dispatcher.dispatched[1], Sequence::Execute(0x1A));
    }
    /// `dispatched` with the pieces of a body joined: how many there are depends on the reads.
    fn with_bodies_joined(dispatched: Vec<Sequence>) -> Vec<Sequence> {
        let mut joined: Vec<Sequence> = Vec::new();
        for sequence in dispatched {
            match (joined.last_mut(), sequence) {
                (Some(Sequence::ApcPut(body)), Sequence::ApcPut(more)) => body.extend(more),
                (_, sequence) => joined.push(sequence),
            }
        }
        joined
    }

    #[test]
    fn apc_is_passed_on_between_its_start_and_end() {
        let dispatched = dispatched_in_reads(b"a\x1b_Gi=1;AAAA\x1b\\b", &[]);

        assert_eq!(dispatched, [
            Sequence::Print('a'),
            Sequence::ApcStart,
            Sequence::ApcPut(b"Gi=1;AAAA".to_vec()),
            Sequence::ApcEnd(false),
            Sequence::Print('b'),
        ]);
    }

    #[test]
    fn an_empty_apc_has_no_body() {
        assert_eq!(dispatched_in_reads(b"\x1b_\x1b\\", &[]), [
            Sequence::ApcStart,
            Sequence::ApcEnd(false),
        ]);
    }

    #[test]
    fn apc_body_comes_in_the_pieces_the_reads_make() {
        const INPUT: &[u8] = b"\x1b_Gabcdef\x1b\\";

        // One read: one piece. Cut in the body: a piece for each read that has any of it.
        assert_eq!(dispatched_in_reads(INPUT, &[]).len(), 3);
        assert_eq!(dispatched_in_reads(INPUT, &[5, 8]), [
            Sequence::ApcStart,
            Sequence::ApcPut(b"Gab".to_vec()),
            Sequence::ApcPut(b"cde".to_vec()),
            Sequence::ApcPut(b"f".to_vec()),
            Sequence::ApcEnd(false),
        ]);
        // A read that ends on the opening and one that starts on the end bring no empty piece.
        assert_eq!(dispatched_in_reads(INPUT, &[2, 3, 9, 10]).len(), 4);
    }

    #[test]
    fn apc_body_is_not_interpreted() {
        // Controls other than the three that end it, `DEL`, UTF-8 and bytes that are not.
        const BODY: &[u8] = b"\x00\x07\x0a\x1f\x7f\x80\x9c\xc3\xa9\xff[1m";
        let mut input = b"\x1b_".to_vec();
        input.extend(BODY);
        input.extend(b"\x1b\\");

        assert_eq!(dispatched_in_reads(&input, &[]), [
            Sequence::ApcStart,
            Sequence::ApcPut(BODY.to_vec()),
            Sequence::ApcEnd(false),
        ]);
    }

    #[test]
    fn apc_cancelled_by_can_and_sub() {
        for cancel in [0x18u8, 0x1A] {
            let input = [b"\x1b_Gx".as_slice(), &[cancel], b"y"].concat();
            assert_eq!(dispatched_in_reads(&input, &[]), [
                Sequence::ApcStart,
                Sequence::ApcPut(b"Gx".to_vec()),
                Sequence::ApcEnd(true),
                Sequence::Execute(cancel),
                Sequence::Print('y'),
            ]);
        }
    }

    #[test]
    fn apc_cancelled_by_an_escape_that_starts_something_else() {
        // The byte after the `ESC` is handled as if the command had not been there.
        assert_eq!(dispatched_in_reads(b"\x1b_Gx\x1b[1mz", &[]), [
            Sequence::ApcStart,
            Sequence::ApcPut(b"Gx".to_vec()),
            Sequence::ApcEnd(true),
            Sequence::Csi(vec![vec![1]], Vec::new(), false, 'm'),
            Sequence::Print('z'),
        ]);
        assert_eq!(dispatched_in_reads(b"\x1b_Gx\x1b_Gy\x1b\\", &[]), [
            Sequence::ApcStart,
            Sequence::ApcPut(b"Gx".to_vec()),
            Sequence::ApcEnd(true),
            Sequence::ApcStart,
            Sequence::ApcPut(b"Gy".to_vec()),
            Sequence::ApcEnd(false),
        ]);
        assert_eq!(dispatched_in_reads(b"\x1b_Gx\x1b\x1b\\", &[]), [
            Sequence::ApcStart,
            Sequence::ApcPut(b"Gx".to_vec()),
            Sequence::ApcEnd(true),
            Sequence::Esc(Vec::new(), false, b'\\'),
        ]);
        assert_eq!(dispatched_in_reads(b"\x1b_Gx\x1b\x18a", &[]), [
            Sequence::ApcStart,
            Sequence::ApcPut(b"Gx".to_vec()),
            Sequence::ApcEnd(true),
            Sequence::Execute(0x18),
            Sequence::Print('a'),
        ]);
    }

    #[test]
    fn sos_and_pm_are_dropped() {
        // Only the `ESC \` that closes each is left, as the string terminator it is.
        assert_eq!(dispatched_in_reads(b"\x1bXabc\x1b\\\x1b^def\x1b\\g", &[]), [
            Sequence::Esc(Vec::new(), false, b'\\'),
            Sequence::Esc(Vec::new(), false, b'\\'),
            Sequence::Print('g'),
        ]);
    }

    #[test]
    fn apc_cut_anywhere_dispatches_what_the_whole_does() {
        // Four commands around text and an escape sequence, two of them cancelled, a body
        // with every kind of byte, and cuts between an `ESC` and its byte.
        const INPUT: &[u8] =
            b"a\x1b_G\x00\x7f\xc3\xa9;QQ\x1b\\\xc2\xb7\x1b[1mb\x1b_Gcut\x18c\x1b_Gx\x1b[0m\x1b_Gok\x1b\\d";

        let whole = with_bodies_joined(dispatched_in_reads(INPUT, &[]));
        assert_eq!(whole.iter().filter(|sequence| **sequence == Sequence::ApcStart).count(), 4);

        for first in 0..=INPUT.len() {
            for second in first..=INPUT.len() {
                assert_eq!(
                    with_bodies_joined(dispatched_in_reads(INPUT, &[first, second])),
                    whole,
                    "cut at {first} and {second}"
                );
            }
        }

        let every_byte: Vec<usize> = (1..INPUT.len()).collect();
        assert_eq!(with_bodies_joined(dispatched_in_reads(INPUT, &every_byte)), whole);
    }
}
