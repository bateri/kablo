// SPDX-License-Identifier: Apache-2.0
//
// This module was originally part of the `alacritty_terminal` crate, which is
// licensed under the Apache License, Version 2.0 and is part of the Alacritty
// project (https://github.com/alacritty/alacritty).

//! ANSI Terminal Stream Parsing.

extern crate alloc;

use alloc::borrow::ToOwned;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::convert::TryFrom;
use core::fmt::{self, Display, Formatter, Write};
#[cfg(feature = "std")]
use core::ops::Mul;
use core::ops::{Add, Sub};
use core::str::FromStr;
use core::time::Duration;
use core::{iter, mem, str};
#[cfg(feature = "std")]
use std::time::Instant;

use bitflags::bitflags;
#[doc(inline)]
pub use cursor_icon;
use cursor_icon::CursorIcon;
use log::debug;
#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

use crate::{Params, ParamsIter};

/// Maximum time before a synchronized update is aborted.
const SYNC_UPDATE_TIMEOUT: Duration = Duration::from_millis(150);

/// Maximum number of bytes read in one synchronized update (2MiB).
const SYNC_BUFFER_SIZE: usize = 0x20_0000;

/// Number of bytes in the BSU/ESU CSI sequences.
const SYNC_ESCAPE_LEN: usize = 8;

/// BSU CSI sequence for beginning or extending synchronized updates.
const BSU_CSI: [u8; SYNC_ESCAPE_LEN] = *b"\x1b[?2026h";

/// ESU CSI sequence for terminating synchronized updates.
const ESU_CSI: [u8; SYNC_ESCAPE_LEN] = *b"\x1b[?2026l";

#[derive(Debug, PartialEq, Eq, Hash)]
pub struct Hyperlink {
    /// Identifier for the given hyperlink.
    pub id: Option<String>,
    /// Resource identifier of the hyperlink.
    pub uri: String,
}

#[derive(Debug, Eq, PartialEq, Copy, Clone, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    /// Implementation of [W3C's luminance algorithm].
    ///
    /// [W3C's luminance algorithm]: https://www.w3.org/TR/WCAG20/#relativeluminancedef
    #[cfg(feature = "std")]
    pub fn luminance(self) -> f64 {
        let channel_luminance = |channel| {
            let channel = channel as f64 / 255.;
            if channel <= 0.03928 {
                channel / 12.92
            } else {
                f64::powf((channel + 0.055) / 1.055, 2.4)
            }
        };

        let r_luminance = channel_luminance(self.r);
        let g_luminance = channel_luminance(self.g);
        let b_luminance = channel_luminance(self.b);

        0.2126 * r_luminance + 0.7152 * g_luminance + 0.0722 * b_luminance
    }

    /// Implementation of [W3C's contrast algorithm].
    ///
    /// [W3C's contrast algorithm]: https://www.w3.org/TR/WCAG20/#contrast-ratiodef
    #[cfg(feature = "std")]
    pub fn contrast(self, other: Rgb) -> f64 {
        let self_luminance = self.luminance();
        let other_luminance = other.luminance();

        let (darker, lighter) = if self_luminance > other_luminance {
            (other_luminance, self_luminance)
        } else {
            (self_luminance, other_luminance)
        };

        (lighter + 0.05) / (darker + 0.05)
    }
}

// A multiply function for Rgb, as the default dim is just *2/3.
#[cfg(feature = "std")]
impl Mul<f32> for Rgb {
    type Output = Rgb;

    fn mul(self, rhs: f32) -> Rgb {
        let result = Rgb {
            r: (f32::from(self.r) * rhs).clamp(0.0, 255.0) as u8,
            g: (f32::from(self.g) * rhs).clamp(0.0, 255.0) as u8,
            b: (f32::from(self.b) * rhs).clamp(0.0, 255.0) as u8,
        };

        log::trace!("Scaling RGB by {} from {:?} to {:?}", rhs, self, result);
        result
    }
}

impl Add<Rgb> for Rgb {
    type Output = Rgb;

    fn add(self, rhs: Rgb) -> Rgb {
        Rgb {
            r: self.r.saturating_add(rhs.r),
            g: self.g.saturating_add(rhs.g),
            b: self.b.saturating_add(rhs.b),
        }
    }
}

impl Sub<Rgb> for Rgb {
    type Output = Rgb;

    fn sub(self, rhs: Rgb) -> Rgb {
        Rgb {
            r: self.r.saturating_sub(rhs.r),
            g: self.g.saturating_sub(rhs.g),
            b: self.b.saturating_sub(rhs.b),
        }
    }
}

impl Display for Rgb {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }
}

impl FromStr for Rgb {
    type Err = ();

    fn from_str(s: &str) -> Result<Rgb, ()> {
        let chars = if s.starts_with("0x") && s.len() == 8 {
            &s[2..]
        } else if s.starts_with('#') && s.len() == 7 {
            &s[1..]
        } else {
            return Err(());
        };

        match u32::from_str_radix(chars, 16) {
            Ok(mut color) => {
                let b = (color & 0xFF) as u8;
                color >>= 8;
                let g = (color & 0xFF) as u8;
                color >>= 8;
                let r = color as u8;
                Ok(Rgb { r, g, b })
            },
            Err(_) => Err(()),
        }
    }
}

/// Parse colors in XParseColor format.
fn xparse_color(color: &[u8]) -> Option<Rgb> {
    if !color.is_empty() && color[0] == b'#' {
        parse_legacy_color(&color[1..])
    } else if color.len() >= 4 && &color[..4] == b"rgb:" {
        parse_rgb_color(&color[4..])
    } else {
        None
    }
}

/// Parse colors in `rgb:r(rrr)/g(ggg)/b(bbb)` format.
fn parse_rgb_color(color: &[u8]) -> Option<Rgb> {
    let colors = str::from_utf8(color).ok()?.split('/').collect::<Vec<_>>();

    if colors.len() != 3 {
        return None;
    }

    // Scale values instead of filling with `0`s.
    let scale = |input: &str| {
        if input.len() > 4 {
            None
        } else {
            let max = u32::pow(16, input.len() as u32) - 1;
            let value = u32::from_str_radix(input, 16).ok()?;
            Some((255 * value / max) as u8)
        }
    };

    Some(Rgb { r: scale(colors[0])?, g: scale(colors[1])?, b: scale(colors[2])? })
}

/// Parse colors in `#r(rrr)g(ggg)b(bbb)` format.
fn parse_legacy_color(color: &[u8]) -> Option<Rgb> {
    let item_len = color.len() / 3;

    // Truncate/Fill to two byte precision.
    let color_from_slice = |slice: &[u8]| {
        let col = usize::from_str_radix(str::from_utf8(slice).ok()?, 16).ok()? << 4;
        Some((col >> (4 * slice.len().saturating_sub(1))) as u8)
    };

    Some(Rgb {
        r: color_from_slice(&color[0..item_len])?,
        g: color_from_slice(&color[item_len..item_len * 2])?,
        b: color_from_slice(&color[item_len * 2..])?,
    })
}

fn parse_number(input: &[u8]) -> Option<u8> {
    if input.is_empty() {
        return None;
    }
    let mut num: u8 = 0;
    for c in input {
        let c = *c as char;
        let digit = c.to_digit(10)?;
        num = num.checked_mul(10).and_then(|v| v.checked_add(digit as u8))?;
    }
    Some(num)
}

/// Internal state for VTE processor.
#[derive(Debug, Default)]
struct ProcessorState<T: Timeout> {
    /// Last processed character for repetition.
    preceding_char: Option<char>,

    /// State for synchronized terminal updates.
    sync_state: SyncState<T>,
}

#[derive(Debug)]
struct SyncState<T: Timeout> {
    /// Handler for synchronized updates.
    timeout: T,

    /// Bytes read during the synchronized update.
    buffer: Vec<u8>,

    /// The replay of `buffer` that halted before it was done, if one did.
    replay: Option<Replay>,
}

impl<T: Timeout> Default for SyncState<T> {
    fn default() -> Self {
        Self {
            buffer: Vec::with_capacity(SYNC_BUFFER_SIZE),
            timeout: Default::default(),
            replay: None,
        }
    }
}

/// A replay of the synchronized bytes that the handler halted, and what is left of it.
///
/// The bytes stay in the sync buffer, which nothing touches until the replay is over.
#[derive(Debug, Clone, Copy)]
struct Replay {
    /// How many bytes of the buffer the parser has been given.
    done: usize,

    /// Where a newer synchronized update starts in the buffer, if one does: the replay
    /// ends there and the buffer keeps the rest. See [`Processor::stop_sync_internal`].
    bsu_offset: Option<usize>,
}

/// The processor wraps a `crate::Parser` to ultimately call methods on a
/// Handler.
#[cfg(feature = "std")]
#[derive(Default)]
pub struct Processor<T: Timeout = StdSyncHandler> {
    state: ProcessorState<T>,
    parser: crate::Parser,
}

/// The processor wraps a `crate::Parser` to ultimately call methods on a
/// Handler.
#[cfg(not(feature = "std"))]
#[derive(Default)]
pub struct Processor<T: Timeout> {
    state: ProcessorState<T>,
    parser: crate::Parser,
}

impl<T: Timeout> Processor<T> {
    #[inline]
    pub fn new() -> Self {
        Self::default()
    }

    /// Synchronized update timeout.
    pub fn sync_timeout(&self) -> &T {
        &self.state.sync_state.timeout
    }

    /// Process a new byte from the PTY.
    ///
    /// Never stops for [`Handler::halt_requested`]: use [`Self::advance_until_halt`] for that.
    #[inline]
    pub fn advance<H>(&mut self, handler: &mut H, bytes: &[u8])
    where
        H: Handler,
    {
        let processed = self.advance_inner(handler, bytes, false);
        debug_assert_eq!(processed, bytes.len());
    }

    /// Process a new byte from the PTY, stopping when the handler asks for it.
    ///
    /// This is [`Self::advance`] that returns right after the byte that made
    /// [`Handler::halt_requested`] true, with the number of bytes it was given: the caller
    /// does what the handler wanted done, then calls this again with the rest of `bytes`.
    /// A call with the handler still halted returns 0 (the handler owns the request and
    /// takes it back, the processor never does).
    ///
    /// The request is not tied to the return value: the last byte of `bytes` can be the
    /// one that made it. Check [`Handler::halt_requested`] after every call, and keep
    /// calling until it is false and `bytes` is used up. That includes an empty `bytes`,
    /// because a halt inside a synchronized update halts its replay and what is left of
    /// the replay goes on in the next call, before any new byte.
    #[inline]
    #[must_use = "Returned value should be used to process the remaining bytes"]
    pub fn advance_until_halt<H>(&mut self, handler: &mut H, bytes: &[u8]) -> usize
    where
        H: Handler,
    {
        self.advance_inner(handler, bytes, true)
    }

    #[inline]
    fn advance_inner<H>(&mut self, handler: &mut H, bytes: &[u8], halting: bool) -> usize
    where
        H: Handler,
    {
        // The handler asked for a halt and has not taken it back: nothing is
        // taken, a synchronized update's buffer included.
        if halting && handler.halt_requested() {
            return 0;
        }

        // What is left of a replay that halted goes before anything new.
        if self.state.sync_state.replay.is_some() && self.resume_replay(handler, halting) {
            return 0;
        }

        let mut processed = 0;
        while processed != bytes.len() {
            if self.state.sync_state.timeout.pending_timeout() {
                let (used, halted) = self.advance_sync(handler, &bytes[processed..], halting);
                processed += used;
                if halted {
                    break;
                }
            } else {
                let mut performer = Performer::new(&mut self.state, handler, true, halting);
                processed +=
                    self.parser.advance_until_terminated(&mut performer, &bytes[processed..]);
                if halting && handler.halt_requested() {
                    break;
                }
            }
        }

        processed
    }

    /// End a synchronized update.
    pub fn stop_sync<H>(&mut self, handler: &mut H)
    where
        H: Handler,
    {
        self.stop_sync_inner(handler, false);
    }

    /// End a synchronized update, stopping when the handler asks for it.
    ///
    /// Returns whether it stopped. If it did the update is not over: do what the handler
    /// wanted done and call [`Self::advance_until_halt`] (an empty slice will do) until
    /// the handler no longer asks, which carries the replay to its end. See there for the
    /// rest of the contract.
    pub fn stop_sync_until_halt<H>(&mut self, handler: &mut H) -> bool
    where
        H: Handler,
    {
        self.stop_sync_inner(handler, true)
    }

    fn stop_sync_inner<H>(&mut self, handler: &mut H, halting: bool) -> bool
    where
        H: Handler,
    {
        // A replay that halted ends first. If it was of an update that a newer one
        // followed, that one is still pending and is what the stop is for.
        if self.state.sync_state.replay.is_some() {
            if self.resume_replay(handler, halting) {
                return true;
            }
            if !self.state.sync_state.timeout.pending_timeout() {
                return false;
            }
        }

        self.stop_sync_internal(handler, None, halting)
    }

    /// End a synchronized update.
    ///
    /// The `bsu_offset` parameter should be passed if the sync buffer contains
    /// a new BSU escape that is not part of the current synchronized
    /// update.
    ///
    /// Returns whether the handler halted the replay of the update.
    fn stop_sync_internal<H>(
        &mut self,
        handler: &mut H,
        bsu_offset: Option<usize>,
        halting: bool,
    ) -> bool
    where
        H: Handler,
    {
        self.state.sync_state.replay = Some(Replay { done: 0, bsu_offset });
        self.resume_replay(handler, halting)
    }

    /// Process the synchronized bytes of a replay that has not run yet or halted.
    ///
    /// Returns whether it halted. When it did, `replay` says where it stands.
    fn resume_replay<H>(&mut self, handler: &mut H, halting: bool) -> bool
    where
        H: Handler,
    {
        let Some(replay) = self.state.sync_state.replay else { return false };

        // Process all synchronized bytes.
        //
        // NOTE: BSU sequences are processed automatically during the synchronized update,
        // so the performer does not stop for them here.
        let buffer = mem::take(&mut self.state.sync_state.buffer);
        let end = replay.bsu_offset.unwrap_or(buffer.len());
        let mut performer = Performer::new(&mut self.state, handler, false, halting);
        let done = replay.done
            + self.parser.advance_until_terminated(&mut performer, &buffer[replay.done..end]);
        self.state.sync_state.buffer = buffer;

        // Not even the last byte is let through: whatever the handler wants done goes before
        // the update is reported over.
        if halting && handler.halt_requested() {
            self.state.sync_state.replay = Some(Replay { done, ..replay });
            return true;
        }
        debug_assert_eq!(done, end);

        self.state.sync_state.replay = None;
        match replay.bsu_offset {
            // Just clear processed bytes if there is a new BSU.
            //
            // NOTE: We do not need to re-process for a new ESU since the `advance_sync`
            // function checks for BSUs in reverse.
            Some(bsu_offset) => {
                let new_len = self.state.sync_state.buffer.len() - bsu_offset;
                self.state.sync_state.buffer.copy_within(bsu_offset.., 0);
                self.state.sync_state.buffer.truncate(new_len);
            },
            // Report mode and clear state if no new BSU is present.
            None => {
                handler.unset_private_mode(NamedPrivateMode::SyncUpdate.into());
                self.state.sync_state.timeout.clear_timeout();
                self.state.sync_state.buffer.clear();
            },
        }

        false
    }

    /// Number of bytes in the synchronization buffer.
    #[inline]
    pub fn sync_bytes_count(&self) -> usize {
        self.state.sync_state.buffer.len()
    }

    /// Process a new byte during a synchronized update.
    ///
    /// Returns the number of bytes processed and whether the handler halted. A halt inside
    /// the replay of the update that this ends leaves the new bytes unprocessed when they
    /// are not part of it (the buffer overflowed), and consumed when they are.
    #[cold]
    fn advance_sync<H>(&mut self, handler: &mut H, bytes: &[u8], halting: bool) -> (usize, bool)
    where
        H: Handler,
    {
        // Advance sync parser or stop sync if we'd exceed the maximum buffer size.
        if self.state.sync_state.buffer.len() + bytes.len() >= SYNC_BUFFER_SIZE - 1 {
            // Terminate the synchronized update.
            if self.stop_sync_internal(handler, None, halting) {
                return (0, true);
            }

            // Just parse the bytes normally.
            let mut performer = Performer::new(&mut self.state, handler, true, halting);
            let used = self.parser.advance_until_terminated(&mut performer, bytes);
            (used, halting && handler.halt_requested())
        } else {
            self.state.sync_state.buffer.extend(bytes);
            let halted = self.advance_sync_csi(handler, bytes.len(), halting);
            (bytes.len(), halted)
        }
    }

    /// Handle BSU/ESU CSI sequences during synchronized update.
    ///
    /// Returns whether the handler halted the replay of an update that ended.
    fn advance_sync_csi<H>(&mut self, handler: &mut H, new_bytes: usize, halting: bool) -> bool
    where
        H: Handler,
    {
        // Get constraints within which a new escape character might be relevant.
        let buffer_len = self.state.sync_state.buffer.len();
        let start_offset = (buffer_len - new_bytes).saturating_sub(SYNC_ESCAPE_LEN - 1);
        let end_offset = buffer_len.saturating_sub(SYNC_ESCAPE_LEN - 1);
        let search_buffer = &self.state.sync_state.buffer[start_offset..end_offset];

        // Search for termination/extension escapes in the added bytes.
        //
        // NOTE: It is technically legal to specify multiple private modes in the same
        // escape, but we only allow EXACTLY `\e[?2026h`/`\e[?2026l` to keep the parser
        // more simple.
        let mut bsu_offset = None;
        for index in memchr::memchr_iter(0x1B, search_buffer).rev() {
            let offset = start_offset + index;
            let escape = &self.state.sync_state.buffer[offset..offset + SYNC_ESCAPE_LEN];

            if escape == BSU_CSI {
                self.state.sync_state.timeout.set_timeout(SYNC_UPDATE_TIMEOUT);
                bsu_offset = Some(offset);
            } else if escape == ESU_CSI {
                return self.stop_sync_internal(handler, bsu_offset, halting);
            }
        }

        false
    }
}

/// Helper type that implements `crate::Perform`.
///
/// Processor creates a Performer when running advance and passes the Performer
/// to `crate::Parser`.
struct Performer<'a, H: Handler, T: Timeout> {
    state: &'a mut ProcessorState<T>,
    handler: &'a mut H,

    /// Whether a BSU was parsed, which terminates the parser if `sync_stops` says so.
    terminated: bool,

    /// Whether the parser stops after a BSU (the bytes after it are the update's).
    sync_stops: bool,

    /// Whether the parser stops when the handler asks for it ([`Handler::halt_requested`]).
    halting: bool,
}

impl<'a, H: Handler + 'a, T: Timeout> Performer<'a, H, T> {
    /// Create a performer.
    #[inline]
    pub fn new<'b>(
        state: &'b mut ProcessorState<T>,
        handler: &'b mut H,
        sync_stops: bool,
        halting: bool,
    ) -> Performer<'b, H, T> {
        Performer { state, handler, terminated: Default::default(), sync_stops, halting }
    }
}

#[cfg(feature = "std")]
#[derive(Default)]
pub struct StdSyncHandler {
    timeout: Option<Instant>,
}

#[cfg(feature = "std")]
impl StdSyncHandler {
    /// Synchronized update expiration time.
    #[inline]
    pub fn sync_timeout(&self) -> Option<Instant> {
        self.timeout
    }
}

#[cfg(feature = "std")]
impl Timeout for StdSyncHandler {
    #[inline]
    fn set_timeout(&mut self, duration: Duration) {
        self.timeout = Some(Instant::now() + duration);
    }

    #[inline]
    fn clear_timeout(&mut self) {
        self.timeout = None;
    }

    #[inline]
    fn pending_timeout(&self) -> bool {
        self.timeout.is_some()
    }
}

/// Interface for creating timeouts and checking their expiry.
///
/// This is internally used by the [`Processor`] to handle synchronized
/// updates.
pub trait Timeout: Default {
    /// Sets the timeout for the next synchronized update.
    ///
    /// The `duration` parameter specifies the duration of the timeout. Once the
    /// specified duration has elapsed, the synchronized update rotuine can be
    /// performed.
    fn set_timeout(&mut self, duration: Duration);
    /// Clear the current timeout.
    fn clear_timeout(&mut self);
    /// Returns whether a timeout is currently active and has not yet expired.
    fn pending_timeout(&self) -> bool;
}

/// Type that handles actions from the parser.
///
/// XXX Should probably not provide default impls for everything, but it makes
/// writing specific handler impls for tests far easier.
pub trait Handler {
    /// OSC to set window title.
    fn set_title(&mut self, _: Option<String>) {}

    /// Set the cursor style.
    fn set_cursor_style(&mut self, _: Option<CursorStyle>) {}

    /// Set the cursor shape.
    fn set_cursor_shape(&mut self, _shape: CursorShape) {}

    /// A character to be displayed.
    fn input(&mut self, _c: char) {}

    /// Set cursor to position.
    fn goto(&mut self, _line: i32, _col: usize) {}

    /// Set cursor to specific row.
    fn goto_line(&mut self, _line: i32) {}

    /// Set cursor to specific column.
    fn goto_col(&mut self, _col: usize) {}

    /// Insert blank characters in current line starting from cursor.
    fn insert_blank(&mut self, _: usize) {}

    /// Move cursor up `rows`.
    fn move_up(&mut self, _: usize) {}

    /// Move cursor down `rows`.
    fn move_down(&mut self, _: usize) {}

    /// Identify the terminal (should write back to the pty stream).
    fn identify_terminal(&mut self, _intermediate: Option<char>) {}

    /// Report device status.
    fn device_status(&mut self, _: usize) {}

    /// Move cursor forward `cols`.
    fn move_forward(&mut self, _col: usize) {}

    /// Move cursor backward `cols`.
    fn move_backward(&mut self, _col: usize) {}

    /// Move cursor down `rows` and set to column 1.
    fn move_down_and_cr(&mut self, _row: usize) {}

    /// Move cursor up `rows` and set to column 1.
    fn move_up_and_cr(&mut self, _row: usize) {}

    /// Put `count` tabs.
    fn put_tab(&mut self, _count: u16) {}

    /// Backspace `count` characters.
    fn backspace(&mut self) {}

    /// Carriage return.
    fn carriage_return(&mut self) {}

    /// Linefeed.
    fn linefeed(&mut self) {}

    /// Ring the bell.
    ///
    /// Hopefully this is never implemented.
    fn bell(&mut self) {}

    /// Substitute char under cursor.
    fn substitute(&mut self) {}

    /// Newline.
    fn newline(&mut self) {}

    /// Set current position as a tabstop.
    fn set_horizontal_tabstop(&mut self) {}

    /// Scroll up `rows` rows.
    fn scroll_up(&mut self, _: usize) {}

    /// Scroll down `rows` rows.
    fn scroll_down(&mut self, _: usize) {}

    /// Insert `count` blank lines.
    fn insert_blank_lines(&mut self, _: usize) {}

    /// Delete `count` lines.
    fn delete_lines(&mut self, _: usize) {}

    /// Erase `count` chars in current line following cursor.
    ///
    /// Erase means resetting to the default state (default colors, no content,
    /// no mode flags).
    fn erase_chars(&mut self, _: usize) {}

    /// Delete `count` chars.
    ///
    /// Deleting a character is like the delete key on the keyboard - everything
    /// to the right of the deleted things is shifted left.
    fn delete_chars(&mut self, _: usize) {}

    /// Move backward `count` tabs.
    fn move_backward_tabs(&mut self, _count: u16) {}

    /// Move forward `count` tabs.
    fn move_forward_tabs(&mut self, _count: u16) {}

    /// Save current cursor position.
    fn save_cursor_position(&mut self) {}

    /// Restore cursor position.
    fn restore_cursor_position(&mut self) {}

    /// Clear current line.
    fn clear_line(&mut self, _mode: LineClearMode) {}

    /// Clear screen.
    fn clear_screen(&mut self, _mode: ClearMode) {}

    /// Clear tab stops.
    fn clear_tabs(&mut self, _mode: TabulationClearMode) {}

    /// Set tab stops at every `interval`.
    fn set_tabs(&mut self, _interval: u16) {}

    /// Reset terminal state.
    fn reset_state(&mut self) {}

    /// Reverse Index.
    ///
    /// Move the active position to the same horizontal position on the
    /// preceding line. If the active position is at the top margin, a scroll
    /// down is performed.
    fn reverse_index(&mut self) {}

    /// Set a terminal attribute.
    fn terminal_attribute(&mut self, _attr: Attr) {}

    /// Set mode.
    fn set_mode(&mut self, _mode: Mode) {}

    /// Unset mode.
    fn unset_mode(&mut self, _mode: Mode) {}

    /// DECRPM - report mode.
    fn report_mode(&mut self, _mode: Mode) {}

    /// Set private mode.
    fn set_private_mode(&mut self, _mode: PrivateMode) {}

    /// Unset private mode.
    fn unset_private_mode(&mut self, _mode: PrivateMode) {}

    /// DECRPM - report private mode.
    fn report_private_mode(&mut self, _mode: PrivateMode) {}

    /// DECSTBM - Set the terminal scrolling region.
    fn set_scrolling_region(&mut self, _top: usize, _bottom: Option<usize>) {}

    /// DECKPAM - Set keypad to applications mode (ESCape instead of digits).
    fn set_keypad_application_mode(&mut self) {}

    /// DECKPNM - Set keypad to numeric mode (digits instead of ESCape seq).
    fn unset_keypad_application_mode(&mut self) {}

    /// Set one of the graphic character sets, G0 to G3, as the active charset.
    ///
    /// 'Invoke' one of G0 to G3 in the GL area. Also referred to as shift in,
    /// shift out and locking shift depending on the set being activated.
    fn set_active_charset(&mut self, _: CharsetIndex) {}

    /// Assign a graphic character set to G0, G1, G2 or G3.
    ///
    /// 'Designate' a graphic character set as one of G0 to G3, so that it can
    /// later be 'invoked' by `set_active_charset`.
    fn configure_charset(&mut self, _: CharsetIndex, _: StandardCharset) {}

    /// Set an indexed color value.
    fn set_color(&mut self, _: usize, _: Rgb) {}

    /// Respond to a color query escape sequence.
    fn dynamic_color_sequence(&mut self, _: String, _: usize, _: &str) {}

    /// Reset an indexed color to original value.
    fn reset_color(&mut self, _: usize) {}

    /// Store data into clipboard.
    fn clipboard_store(&mut self, _: u8, _: &[u8]) {}

    /// Load data from clipboard.
    fn clipboard_load(&mut self, _: u8, _: &str) {}

    /// Run the decaln routine.
    fn decaln(&mut self) {}

    /// Push a title onto the stack.
    fn push_title(&mut self) {}

    /// Pop the last title from the stack.
    fn pop_title(&mut self) {}

    /// Report text area size in pixels.
    fn text_area_size_pixels(&mut self) {}

    /// Report text area size in characters.
    fn text_area_size_chars(&mut self) {}

    /// Set hyperlink.
    fn set_hyperlink(&mut self, _: Option<Hyperlink>) {}

    /// Set mouse cursor icon.
    fn set_mouse_cursor_icon(&mut self, _: CursorIcon) {}

    /// Set the mouse pointer's shape (OSC 22) from the request as the program wrote
    /// it: an optional operation (`=`, `>`, `<`, `?`) and a comma-separated list of
    /// names, possibly empty. `terminator` is the one a reply takes. By default a
    /// single name is read and set, as upstream reads the sequence.
    fn set_pointer_shape(&mut self, request: &[u8], _terminator: &str) {
        let shape = String::from_utf8_lossy(request);
        match CursorIcon::from_str(&shape) {
            Ok(cursor_icon) => self.set_mouse_cursor_icon(cursor_icon),
            Err(_) => debug!("[osc 22] unrecognized cursor icon shape: {shape:?}"),
        }
    }

    /// Report current keyboard mode.
    fn report_keyboard_mode(&mut self) {}

    /// Push keyboard mode into the keyboard mode stack.
    fn push_keyboard_mode(&mut self, _mode: KeyboardModes) {}

    /// Pop the given amount of keyboard modes from the
    /// keyboard mode stack.
    fn pop_keyboard_modes(&mut self, _to_pop: u16) {}

    /// Set the [`keyboard mode`] using the given [`behavior`].
    ///
    /// [`keyboard mode`]: crate::ansi::KeyboardModes
    /// [`behavior`]: crate::ansi::KeyboardModesApplyBehavior
    fn set_keyboard_mode(&mut self, _mode: KeyboardModes, _behavior: KeyboardModesApplyBehavior) {}

    /// Set XTerm's [`ModifyOtherKeys`] option.
    fn set_modify_other_keys(&mut self, _mode: ModifyOtherKeys) {}

    /// Report XTerm's [`ModifyOtherKeys`] state.
    ///
    /// The output is of form `CSI > 4 ; mode m`.
    fn report_modify_other_keys(&mut self) {}

    // Set SCP control.
    fn set_scp(&mut self, _char_path: ScpCharPath, _update_mode: ScpUpdateMode) {}

    /// An application program command (`ESC _ … ESC \`) begins.
    ///
    /// See [`crate::Perform::apc_start`] for the sequence of calls that follows.
    fn apc_start(&mut self) {}

    /// A piece of the body of the command begun by [`Handler::apc_start`].
    fn apc_put(&mut self, _data: &[u8]) {}

    /// The command ended; `cancelled` is `false` when `ESC \` completed it.
    fn apc_end(&mut self, _cancelled: bool) {}

    /// Whether the handler wants [`Processor::advance_until_halt`] to return.
    ///
    /// Asked after every byte the parser reads, so it is a flag read and nothing more.
    /// What the handler wants done in the pause (work it should not do while the caller
    /// holds a lock, say) is its own business, and so is taking the request back:
    /// while it stays true the processor takes no more bytes. [`Processor::advance`] never
    /// asks.
    #[inline(always)]
    fn halt_requested(&self) -> bool {
        false
    }
}

bitflags! {
    /// A set of [`kitty keyboard protocol'] modes.
    ///
    /// [`kitty keyboard protocol']: https://sw.kovidgoyal.net/kitty/keyboard-protocol
    #[derive(Default, Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct KeyboardModes : u8 {
        /// No keyboard protocol mode is set.
        const NO_MODE                 = 0b0000_0000;
        /// Report `Esc`, `alt` + `key`, `ctrl` + `key`, `ctrl` + `alt` + `key`, `shift`
        /// + `alt` + `key` keys using `CSI u` sequence instead of raw ones.
        const DISAMBIGUATE_ESC_CODES  = 0b0000_0001;
        /// Report key presses, release, and repetition alongside the escape. Key events
        /// that result in text are reported as plain UTF-8, unless the
        /// [`Self::REPORT_ALL_KEYS_AS_ESC`] is enabled.
        const REPORT_EVENT_TYPES      = 0b0000_0010;
        /// Additionally report shifted key an dbase layout key.
        const REPORT_ALTERNATE_KEYS   = 0b0000_0100;
        /// Report every key as an escape sequence.
        const REPORT_ALL_KEYS_AS_ESC  = 0b0000_1000;
        /// Report the text generated by the key event.
        const REPORT_ASSOCIATED_TEXT  = 0b0001_0000;
    }
}

/// XTMODKEYS modifyOtherKeys state.
///
/// This only applies to keys corresponding to ascii characters.
///
/// For the details on how to implement the mode handling correctly, consult
/// [`XTerm's implementation`] and the [`output`] of XTerm's provided [`perl
/// script`]. Some libraries and implementations also use the [`fixterms`]
/// definition of the `CSI u`.
///
/// The end escape sequence has a `CSI char; modifiers u` form while the
/// original `CSI 27 ; modifier ; char ~`. The clients should prefer the `CSI
/// u`, since it has more adoption.
///
/// [`XTerm's implementation`]: https://invisible-island.net/xterm/modified-keys.html
/// [`perl script`]: https://github.com/ThomasDickey/xterm-snapshots/blob/master/vttests/modify-keys.pl
/// [`output`]: https://github.com/alacritty/vte/blob/master/doc/modifyOtherKeys-example.txt
/// [`fixterms`]: http://www.leonerd.org.uk/hacks/fixterms/
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModifyOtherKeys {
    /// Reset the state.
    Reset,
    /// Enables this feature except for keys with well-known behavior, e.g.,
    /// Tab, Backspace and some special control character cases which are
    /// built into the X11 library (e.g., Control-Space to make a NUL, or
    /// Control-3 to make an Escape character).
    ///
    /// Escape sequences shouldn't be emitted under the following circumstances:
    /// - When the key is in range of `[64;127]` and the modifier is either
    ///   Control or Shift
    /// - When the key combination is a known control combination alias
    ///
    /// For more details, consult the [`example`] for the suggested translation.
    ///
    /// [`example`]: https://github.com/alacritty/vte/blob/master/doc/modifyOtherKeys-example.txt
    EnableExceptWellDefined,
    /// Enables this feature for all keys including the exceptions of
    /// [`Self::EnableExceptWellDefined`].  XTerm still ignores the special
    /// cases built into the X11 library. Any shifted (modified) ordinary
    /// key send an escape sequence. The Alt- and Meta- modifiers cause
    /// XTerm to send escape sequences.
    ///
    /// For more details, consult the [`example`] for the suggested translation.
    ///
    /// [`example`]: https://github.com/alacritty/vte/blob/master/doc/modifyOtherKeys-example.txt
    EnableAll,
}

/// Describes how the new [`KeyboardModes`] should be applied.
#[repr(u8)]
#[derive(Default, Clone, Copy, PartialEq, Eq)]
pub enum KeyboardModesApplyBehavior {
    /// Replace the active flags with the new ones.
    #[default]
    Replace,
    /// Merge the given flags with currently active ones.
    Union,
    /// Remove the given flags from the active ones.
    Difference,
}

/// Terminal cursor configuration.
#[derive(Default, Debug, Eq, PartialEq, Copy, Clone, Hash)]
pub struct CursorStyle {
    pub shape: CursorShape,
    pub blinking: bool,
}

/// Terminal cursor shape.
#[derive(Debug, Default, Eq, PartialEq, Copy, Clone, Hash)]
pub enum CursorShape {
    /// Cursor is a block like `▒`.
    #[default]
    Block,

    /// Cursor is an underscore like `_`.
    Underline,

    /// Cursor is a vertical bar `⎸`.
    Beam,

    /// Cursor is a box like `☐`.
    HollowBlock,

    /// Invisible cursor.
    Hidden,
}

/// Wrapper for the ANSI modes.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Mode {
    /// Known ANSI mode.
    Named(NamedMode),
    /// Unidentified publc mode.
    Unknown(u16),
}

impl Mode {
    fn new(mode: u16) -> Self {
        match mode {
            4 => Self::Named(NamedMode::Insert),
            20 => Self::Named(NamedMode::LineFeedNewLine),
            _ => Self::Unknown(mode),
        }
    }

    /// Get the raw value of the mode.
    pub fn raw(self) -> u16 {
        match self {
            Self::Named(named) => named as u16,
            Self::Unknown(mode) => mode,
        }
    }
}

impl From<NamedMode> for Mode {
    fn from(value: NamedMode) -> Self {
        Self::Named(value)
    }
}

/// ANSI modes.
#[repr(u16)]
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum NamedMode {
    /// IRM Insert Mode.
    Insert = 4,
    LineFeedNewLine = 20,
}

/// Wrapper for the private DEC modes.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum PrivateMode {
    /// Known private mode.
    Named(NamedPrivateMode),
    /// Unknown private mode.
    Unknown(u16),
}

impl PrivateMode {
    fn new(mode: u16) -> Self {
        match mode {
            1 => Self::Named(NamedPrivateMode::CursorKeys),
            3 => Self::Named(NamedPrivateMode::ColumnMode),
            6 => Self::Named(NamedPrivateMode::Origin),
            7 => Self::Named(NamedPrivateMode::LineWrap),
            12 => Self::Named(NamedPrivateMode::BlinkingCursor),
            25 => Self::Named(NamedPrivateMode::ShowCursor),
            1000 => Self::Named(NamedPrivateMode::ReportMouseClicks),
            1002 => Self::Named(NamedPrivateMode::ReportCellMouseMotion),
            1003 => Self::Named(NamedPrivateMode::ReportAllMouseMotion),
            1004 => Self::Named(NamedPrivateMode::ReportFocusInOut),
            1005 => Self::Named(NamedPrivateMode::Utf8Mouse),
            1006 => Self::Named(NamedPrivateMode::SgrMouse),
            1007 => Self::Named(NamedPrivateMode::AlternateScroll),
            1042 => Self::Named(NamedPrivateMode::UrgencyHints),
            1049 => Self::Named(NamedPrivateMode::SwapScreenAndSetRestoreCursor),
            2004 => Self::Named(NamedPrivateMode::BracketedPaste),
            2026 => Self::Named(NamedPrivateMode::SyncUpdate),
            _ => Self::Unknown(mode),
        }
    }

    /// Get the raw value of the mode.
    pub fn raw(self) -> u16 {
        match self {
            Self::Named(named) => named as u16,
            Self::Unknown(mode) => mode,
        }
    }
}

impl From<NamedPrivateMode> for PrivateMode {
    fn from(value: NamedPrivateMode) -> Self {
        Self::Named(value)
    }
}

/// Private DEC modes.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum NamedPrivateMode {
    CursorKeys = 1,
    /// Select 80 or 132 columns per page (DECCOLM).
    ///
    /// CSI ? 3 h -> set 132 column font.
    /// CSI ? 3 l -> reset 80 column font.
    ///
    /// Additionally,
    ///
    /// * set margins to default positions
    /// * erases all data in page memory
    /// * resets DECLRMM to unavailable
    /// * clears data from the status line (if set to host-writable)
    ColumnMode = 3,
    Origin = 6,
    LineWrap = 7,
    BlinkingCursor = 12,
    ShowCursor = 25,
    ReportMouseClicks = 1000,
    ReportCellMouseMotion = 1002,
    ReportAllMouseMotion = 1003,
    ReportFocusInOut = 1004,
    Utf8Mouse = 1005,
    SgrMouse = 1006,
    AlternateScroll = 1007,
    UrgencyHints = 1042,
    SwapScreenAndSetRestoreCursor = 1049,
    BracketedPaste = 2004,
    /// The mode is handled automatically by [`Processor`].
    SyncUpdate = 2026,
}

/// Mode for clearing line.
///
/// Relative to cursor.
#[derive(Debug)]
pub enum LineClearMode {
    /// Clear right of cursor.
    Right,
    /// Clear left of cursor.
    Left,
    /// Clear entire line.
    All,
}

/// Mode for clearing terminal.
///
/// Relative to cursor.
#[derive(Debug)]
pub enum ClearMode {
    /// Clear below cursor.
    Below,
    /// Clear above cursor.
    Above,
    /// Clear entire terminal.
    All,
    /// Clear 'saved' lines (scrollback).
    Saved,
}

/// Mode for clearing tab stops.
#[derive(Debug)]
pub enum TabulationClearMode {
    /// Clear stop under cursor.
    Current,
    /// Clear all stops.
    All,
}

/// Standard colors.
///
/// The order here matters since the enum should be castable to a `usize` for
/// indexing a color list.
#[derive(Debug, Copy, Clone, Eq, PartialEq, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum NamedColor {
    /// Black.
    Black = 0,
    /// Red.
    Red,
    /// Green.
    Green,
    /// Yellow.
    Yellow,
    /// Blue.
    Blue,
    /// Magenta.
    Magenta,
    /// Cyan.
    Cyan,
    /// White.
    White,
    /// Bright black.
    BrightBlack,
    /// Bright red.
    BrightRed,
    /// Bright green.
    BrightGreen,
    /// Bright yellow.
    BrightYellow,
    /// Bright blue.
    BrightBlue,
    /// Bright magenta.
    BrightMagenta,
    /// Bright cyan.
    BrightCyan,
    /// Bright white.
    BrightWhite,
    /// The foreground color.
    Foreground = 256,
    /// The background color.
    Background,
    /// Color for the cursor itself.
    Cursor,
    /// Dim black.
    DimBlack,
    /// Dim red.
    DimRed,
    /// Dim green.
    DimGreen,
    /// Dim yellow.
    DimYellow,
    /// Dim blue.
    DimBlue,
    /// Dim magenta.
    DimMagenta,
    /// Dim cyan.
    DimCyan,
    /// Dim white.
    DimWhite,
    /// The bright foreground color.
    BrightForeground,
    /// Dim foreground.
    DimForeground,
}

impl NamedColor {
    #[must_use]
    pub fn to_bright(self) -> Self {
        match self {
            NamedColor::Foreground => NamedColor::BrightForeground,
            NamedColor::Black => NamedColor::BrightBlack,
            NamedColor::Red => NamedColor::BrightRed,
            NamedColor::Green => NamedColor::BrightGreen,
            NamedColor::Yellow => NamedColor::BrightYellow,
            NamedColor::Blue => NamedColor::BrightBlue,
            NamedColor::Magenta => NamedColor::BrightMagenta,
            NamedColor::Cyan => NamedColor::BrightCyan,
            NamedColor::White => NamedColor::BrightWhite,
            NamedColor::DimForeground => NamedColor::Foreground,
            NamedColor::DimBlack => NamedColor::Black,
            NamedColor::DimRed => NamedColor::Red,
            NamedColor::DimGreen => NamedColor::Green,
            NamedColor::DimYellow => NamedColor::Yellow,
            NamedColor::DimBlue => NamedColor::Blue,
            NamedColor::DimMagenta => NamedColor::Magenta,
            NamedColor::DimCyan => NamedColor::Cyan,
            NamedColor::DimWhite => NamedColor::White,
            val => val,
        }
    }

    #[must_use]
    pub fn to_dim(self) -> Self {
        match self {
            NamedColor::Black => NamedColor::DimBlack,
            NamedColor::Red => NamedColor::DimRed,
            NamedColor::Green => NamedColor::DimGreen,
            NamedColor::Yellow => NamedColor::DimYellow,
            NamedColor::Blue => NamedColor::DimBlue,
            NamedColor::Magenta => NamedColor::DimMagenta,
            NamedColor::Cyan => NamedColor::DimCyan,
            NamedColor::White => NamedColor::DimWhite,
            NamedColor::Foreground => NamedColor::DimForeground,
            NamedColor::BrightBlack => NamedColor::Black,
            NamedColor::BrightRed => NamedColor::Red,
            NamedColor::BrightGreen => NamedColor::Green,
            NamedColor::BrightYellow => NamedColor::Yellow,
            NamedColor::BrightBlue => NamedColor::Blue,
            NamedColor::BrightMagenta => NamedColor::Magenta,
            NamedColor::BrightCyan => NamedColor::Cyan,
            NamedColor::BrightWhite => NamedColor::White,
            NamedColor::BrightForeground => NamedColor::Foreground,
            val => val,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum Color {
    Named(NamedColor),
    Spec(Rgb),
    Indexed(u8),
}

/// Terminal character attributes.
#[derive(Debug, Eq, PartialEq)]
pub enum Attr {
    /// Clear all special abilities.
    Reset,
    /// Bold text.
    Bold,
    /// Dim or secondary color.
    Dim,
    /// Italic text.
    Italic,
    /// Underline text.
    Underline,
    /// Underlined twice.
    DoubleUnderline,
    /// Undercurled text.
    Undercurl,
    /// Dotted underlined text.
    DottedUnderline,
    /// Dashed underlined text.
    DashedUnderline,
    /// Blink cursor slowly.
    BlinkSlow,
    /// Blink cursor fast.
    BlinkFast,
    /// Invert colors.
    Reverse,
    /// Do not display characters.
    Hidden,
    /// Strikeout text.
    Strike,
    /// Cancel bold.
    CancelBold,
    /// Cancel bold and dim.
    CancelBoldDim,
    /// Cancel italic.
    CancelItalic,
    /// Cancel all underlines.
    CancelUnderline,
    /// Cancel blink.
    CancelBlink,
    /// Cancel inversion.
    CancelReverse,
    /// Cancel text hiding.
    CancelHidden,
    /// Cancel strikeout.
    CancelStrike,
    /// Set indexed foreground color.
    Foreground(Color),
    /// Set indexed background color.
    Background(Color),
    /// Underline color.
    UnderlineColor(Option<Color>),
}

/// Identifiers which can be assigned to a graphic character set.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CharsetIndex {
    /// Default set, is designated as ASCII at startup.
    #[default]
    G0,
    G1,
    G2,
    G3,
}

/// Standard or common character sets which can be designated as G0-G3.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum StandardCharset {
    #[default]
    Ascii,
    SpecialCharacterAndLineDrawing,
}

impl StandardCharset {
    /// Switch/Map character to the active charset. Ascii is the common case and
    /// for that we want to do as little as possible.
    #[inline]
    pub fn map(self, c: char) -> char {
        match self {
            StandardCharset::Ascii => c,
            StandardCharset::SpecialCharacterAndLineDrawing => match c {
                '_' => ' ',
                '`' => '◆',
                'a' => '▒',
                'b' => '\u{2409}', // Symbol for horizontal tabulation
                'c' => '\u{240c}', // Symbol for form feed
                'd' => '\u{240d}', // Symbol for carriage return
                'e' => '\u{240a}', // Symbol for line feed
                'f' => '°',
                'g' => '±',
                'h' => '\u{2424}', // Symbol for newline
                'i' => '\u{240b}', // Symbol for vertical tabulation
                'j' => '┘',
                'k' => '┐',
                'l' => '┌',
                'm' => '└',
                'n' => '┼',
                'o' => '⎺',
                'p' => '⎻',
                'q' => '─',
                'r' => '⎼',
                's' => '⎽',
                't' => '├',
                'u' => '┤',
                'v' => '┴',
                'w' => '┬',
                'x' => '│',
                'y' => '≤',
                'z' => '≥',
                '{' => 'π',
                '|' => '≠',
                '}' => '£',
                '~' => '·',
                _ => c,
            },
        }
    }
}

/// SCP control's first parameter which determines character path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScpCharPath {
    /// SCP's first parameter value of 0. Behavior is implementation defined.
    Default,
    /// SCP's first parameter value of 1 which sets character path to
    /// LEFT-TO-RIGHT.
    LTR,
    /// SCP's first parameter value of 2 which sets character path to
    /// RIGHT-TO-LEFT.
    RTL,
}

/// SCP control's second parameter which determines update mode/direction
/// between components.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScpUpdateMode {
    /// SCP's second parameter value of 0 (the default). Implementation
    /// dependant update.
    ImplementationDependant,
    /// SCP's second parameter value of 1.
    ///
    /// Reflect data component changes in the presentation component.
    DataToPresentation,
    /// SCP's second parameter value of 2.
    ///
    /// Reflect presentation component changes in the data component.
    PresentationToData,
}

impl<'a, H, T> crate::Perform for Performer<'a, H, T>
where
    H: Handler + 'a,
    T: Timeout,
{
    #[inline]
    fn print(&mut self, c: char) {
        self.handler.input(c);
        self.state.preceding_char = Some(c);
    }

    #[inline]
    fn execute(&mut self, byte: u8) {
        match byte {
            C0::HT => self.handler.put_tab(1),
            C0::BS => self.handler.backspace(),
            C0::CR => self.handler.carriage_return(),
            C0::LF | C0::VT | C0::FF => self.handler.linefeed(),
            C0::BEL => self.handler.bell(),
            C0::SUB => self.handler.substitute(),
            C0::SI => self.handler.set_active_charset(CharsetIndex::G0),
            C0::SO => self.handler.set_active_charset(CharsetIndex::G1),
            _ => debug!("[unhandled] execute byte={:02x}", byte),
        }
    }

    #[inline]
    fn hook(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        debug!(
            "[unhandled hook] params={:?}, ints: {:?}, ignore: {:?}, action: {:?}",
            params, intermediates, ignore, action
        );
    }

    #[inline]
    fn put(&mut self, byte: u8) {
        debug!("[unhandled put] byte={:?}", byte);
    }

    #[inline]
    fn unhook(&mut self) {
        debug!("[unhandled unhook]");
    }

    #[inline]
    fn apc_start(&mut self) {
        self.handler.apc_start();
    }

    #[inline]
    fn apc_put(&mut self, data: &[u8]) {
        self.handler.apc_put(data);
    }

    #[inline]
    fn apc_end(&mut self, cancelled: bool) {
        self.handler.apc_end(cancelled);
    }

    #[inline]
    fn osc_dispatch(&mut self, params: &[&[u8]], bell_terminated: bool) {
        let terminator = if bell_terminated { "\x07" } else { "\x1b\\" };

        fn unhandled(params: &[&[u8]]) {
            let mut buf = String::new();
            for items in params {
                buf.push('[');
                for item in *items {
                    let _ = write!(buf, "{:?}", *item as char);
                }
                buf.push_str("],");
            }
            debug!("[unhandled osc_dispatch]: [{}] at line {}", &buf, line!());
        }

        if params.is_empty() || params[0].is_empty() {
            return;
        }

        match params[0] {
            // Set window title.
            b"0" | b"2" => {
                if params.len() >= 2 {
                    let title = params[1..]
                        .iter()
                        .flat_map(|x| str::from_utf8(x))
                        .collect::<Vec<&str>>()
                        .join(";")
                        .trim()
                        .to_owned();
                    self.handler.set_title(Some(title));
                    return;
                }
                unhandled(params);
            },

            // Set color index.
            b"4" => {
                if params.len() <= 1 || params.len() % 2 == 0 {
                    unhandled(params);
                    return;
                }

                for chunk in params[1..].chunks(2) {
                    let index = match parse_number(chunk[0]) {
                        Some(index) => index,
                        None => {
                            unhandled(params);
                            continue;
                        },
                    };

                    if let Some(c) = xparse_color(chunk[1]) {
                        self.handler.set_color(index as usize, c);
                    } else if chunk[1] == b"?" {
                        let prefix = alloc::format!("4;{index}");
                        self.handler.dynamic_color_sequence(prefix, index as usize, terminator);
                    } else {
                        unhandled(params);
                    }
                }
            },

            // Hyperlink.
            b"8" if params.len() > 2 => {
                let link_params = params[1];

                // NOTE: The escape sequence is of form 'OSC 8 ; params ; URI ST', where
                // URI is URL-encoded. However `;` is a special character and might be
                // passed as is, thus we need to rebuild the URI.
                let mut uri = str::from_utf8(params[2]).unwrap_or_default().to_string();
                for param in params[3..].iter() {
                    uri.push(';');
                    uri.push_str(str::from_utf8(param).unwrap_or_default());
                }

                // The OSC 8 escape sequence must be stopped when getting an empty `uri`.
                if uri.is_empty() {
                    self.handler.set_hyperlink(None);
                    return;
                }

                // Link parameters are in format of `key1=value1:key2=value2`. Currently only
                // key `id` is defined.
                let id = link_params
                    .split(|&b| b == b':')
                    .find_map(|kv| kv.strip_prefix(b"id="))
                    .and_then(|kv| str::from_utf8(kv).ok().map(|e| e.to_owned()));

                self.handler.set_hyperlink(Some(Hyperlink { id, uri }));
            },

            // Get/set Foreground, Background, Cursor colors.
            b"10" | b"11" | b"12" => {
                if params.len() >= 2 {
                    if let Some(mut dynamic_code) = parse_number(params[0]) {
                        for param in &params[1..] {
                            // 10 is the first dynamic color, also the foreground.
                            let offset = dynamic_code as usize - 10;
                            let index = NamedColor::Foreground as usize + offset;

                            // End of setting dynamic colors.
                            if index > NamedColor::Cursor as usize {
                                unhandled(params);
                                break;
                            }

                            if let Some(color) = xparse_color(param) {
                                self.handler.set_color(index, color);
                            } else if param == b"?" {
                                self.handler.dynamic_color_sequence(
                                    dynamic_code.to_string(),
                                    index,
                                    terminator,
                                );
                            } else {
                                unhandled(params);
                            }
                            dynamic_code += 1;
                        }
                        return;
                    }
                }
                unhandled(params);
            },

            // Set mouse cursor shape.
            b"22" if params.len() <= 2 => {
                let request = params.get(1).copied().unwrap_or_default();
                self.handler.set_pointer_shape(request, terminator);
            },

            // Set cursor style.
            b"50" => {
                if params.len() >= 2
                    && params[1].len() >= 13
                    && params[1][0..12] == *b"CursorShape="
                {
                    let shape = match params[1][12] as char {
                        '0' => CursorShape::Block,
                        '1' => CursorShape::Beam,
                        '2' => CursorShape::Underline,
                        _ => return unhandled(params),
                    };
                    self.handler.set_cursor_shape(shape);
                    return;
                }
                unhandled(params);
            },

            // Set clipboard.
            b"52" => {
                if params.len() < 3 {
                    return unhandled(params);
                }

                let clipboard = params[1].first().unwrap_or(&b'c');
                match params[2] {
                    b"?" => self.handler.clipboard_load(*clipboard, terminator),
                    base64 => self.handler.clipboard_store(*clipboard, base64),
                }
            },

            // Reset color index.
            b"104" => {
                // Reset all color indexes when no parameters are given.
                if params.len() == 1 || params[1].is_empty() {
                    for i in 0..256 {
                        self.handler.reset_color(i);
                    }
                    return;
                }

                // Reset color indexes given as parameters.
                for param in &params[1..] {
                    match parse_number(param) {
                        Some(index) => self.handler.reset_color(index as usize),
                        None => unhandled(params),
                    }
                }
            },

            // Reset foreground color.
            b"110" => self.handler.reset_color(NamedColor::Foreground as usize),

            // Reset background color.
            b"111" => self.handler.reset_color(NamedColor::Background as usize),

            // Reset text cursor color.
            b"112" => self.handler.reset_color(NamedColor::Cursor as usize),

            _ => unhandled(params),
        }
    }

    #[allow(clippy::cognitive_complexity)]
    #[inline]
    fn csi_dispatch(
        &mut self,
        params: &Params,
        intermediates: &[u8],
        has_ignored_intermediates: bool,
        action: char,
    ) {
        macro_rules! unhandled {
            () => {{
                debug!(
                    "[Unhandled CSI] action={:?}, params={:?}, intermediates={:?}",
                    action, params, intermediates
                );
            }};
        }

        if has_ignored_intermediates || intermediates.len() > 2 {
            unhandled!();
            return;
        }

        let mut params_iter = params.iter();
        let handler = &mut self.handler;

        let mut next_param_or = |default: u16| match params_iter.next() {
            Some(&[param, ..]) if param != 0 => param,
            _ => default,
        };

        match (action, intermediates) {
            ('@', []) => handler.insert_blank(next_param_or(1) as usize),
            ('A', []) => handler.move_up(next_param_or(1) as usize),
            ('B', []) | ('e', []) => handler.move_down(next_param_or(1) as usize),
            ('b', []) => {
                if let Some(c) = self.state.preceding_char {
                    for _ in 0..next_param_or(1) {
                        handler.input(c);
                    }
                } else {
                    debug!("tried to repeat with no preceding char");
                }
            },
            ('C', []) | ('a', []) => handler.move_forward(next_param_or(1) as usize),
            ('c', intermediates) if next_param_or(0) == 0 => {
                handler.identify_terminal(intermediates.first().map(|&i| i as char))
            },
            ('D', []) => handler.move_backward(next_param_or(1) as usize),
            ('d', []) => handler.goto_line(next_param_or(1) as i32 - 1),
            ('E', []) => handler.move_down_and_cr(next_param_or(1) as usize),
            ('F', []) => handler.move_up_and_cr(next_param_or(1) as usize),
            ('G', []) | ('`', []) => handler.goto_col(next_param_or(1) as usize - 1),
            ('W', [b'?']) if next_param_or(0) == 5 => handler.set_tabs(8),
            ('g', []) => {
                let mode = match next_param_or(0) {
                    0 => TabulationClearMode::Current,
                    3 => TabulationClearMode::All,
                    _ => {
                        unhandled!();
                        return;
                    },
                };

                handler.clear_tabs(mode);
            },
            ('H', []) | ('f', []) => {
                let y = next_param_or(1) as i32;
                let x = next_param_or(1) as usize;
                handler.goto(y - 1, x - 1);
            },
            ('h', []) => {
                for param in params_iter.map(|param| param[0]) {
                    handler.set_mode(Mode::new(param))
                }
            },
            ('h', [b'?']) => {
                for param in params_iter.map(|param| param[0]) {
                    // Handle sync updates opaquely.
                    if param == NamedPrivateMode::SyncUpdate as u16 {
                        self.state.sync_state.timeout.set_timeout(SYNC_UPDATE_TIMEOUT);
                        self.terminated = true;
                    }

                    handler.set_private_mode(PrivateMode::new(param))
                }
            },
            ('I', []) => handler.move_forward_tabs(next_param_or(1)),
            ('J', []) => {
                let mode = match next_param_or(0) {
                    0 => ClearMode::Below,
                    1 => ClearMode::Above,
                    2 => ClearMode::All,
                    3 => ClearMode::Saved,
                    _ => {
                        unhandled!();
                        return;
                    },
                };

                handler.clear_screen(mode);
            },
            ('K', []) => {
                let mode = match next_param_or(0) {
                    0 => LineClearMode::Right,
                    1 => LineClearMode::Left,
                    2 => LineClearMode::All,
                    _ => {
                        unhandled!();
                        return;
                    },
                };

                handler.clear_line(mode);
            },
            ('k', [b' ']) => {
                // SCP control.
                let char_path = match next_param_or(0) {
                    0 => ScpCharPath::Default,
                    1 => ScpCharPath::LTR,
                    2 => ScpCharPath::RTL,
                    _ => {
                        unhandled!();
                        return;
                    },
                };

                let update_mode = match next_param_or(0) {
                    0 => ScpUpdateMode::ImplementationDependant,
                    1 => ScpUpdateMode::DataToPresentation,
                    2 => ScpUpdateMode::PresentationToData,
                    _ => {
                        unhandled!();
                        return;
                    },
                };

                handler.set_scp(char_path, update_mode);
            },
            ('L', []) => handler.insert_blank_lines(next_param_or(1) as usize),
            ('l', []) => {
                for param in params_iter.map(|param| param[0]) {
                    handler.unset_mode(Mode::new(param))
                }
            },
            ('l', [b'?']) => {
                for param in params_iter.map(|param| param[0]) {
                    handler.unset_private_mode(PrivateMode::new(param))
                }
            },
            ('M', []) => handler.delete_lines(next_param_or(1) as usize),
            ('m', []) => {
                if params.is_empty() {
                    handler.terminal_attribute(Attr::Reset);
                } else {
                    attrs_from_sgr_parameters(*handler, &mut params_iter);
                }
            },
            ('m', [b'>']) => {
                let mode = match (next_param_or(1) == 4).then(|| next_param_or(0)) {
                    Some(0) => ModifyOtherKeys::Reset,
                    Some(1) => ModifyOtherKeys::EnableExceptWellDefined,
                    Some(2) => ModifyOtherKeys::EnableAll,
                    _ => return unhandled!(),
                };
                handler.set_modify_other_keys(mode);
            },
            ('m', [b'?']) => {
                if params_iter.next() == Some(&[4]) {
                    handler.report_modify_other_keys();
                } else {
                    unhandled!()
                }
            },
            ('n', []) => handler.device_status(next_param_or(0) as usize),
            ('P', []) => handler.delete_chars(next_param_or(1) as usize),
            ('p', [b'$']) => {
                let mode = next_param_or(0);
                handler.report_mode(Mode::new(mode));
            },
            ('p', [b'?', b'$']) => {
                let mode = next_param_or(0);
                handler.report_private_mode(PrivateMode::new(mode));
            },
            ('q', [b' ']) => {
                // DECSCUSR (CSI Ps SP q) -- Set Cursor Style.
                let cursor_style_id = next_param_or(0);
                let shape = match cursor_style_id {
                    0 => None,
                    1 | 2 => Some(CursorShape::Block),
                    3 | 4 => Some(CursorShape::Underline),
                    5 | 6 => Some(CursorShape::Beam),
                    _ => {
                        unhandled!();
                        return;
                    },
                };
                let cursor_style =
                    shape.map(|shape| CursorStyle { shape, blinking: cursor_style_id % 2 == 1 });

                handler.set_cursor_style(cursor_style);
            },
            ('r', []) => {
                let top = next_param_or(1) as usize;
                let bottom =
                    params_iter.next().map(|param| param[0] as usize).filter(|&param| param != 0);

                handler.set_scrolling_region(top, bottom);
            },
            ('S', []) => handler.scroll_up(next_param_or(1) as usize),
            ('s', []) => handler.save_cursor_position(),
            ('T', []) => handler.scroll_down(next_param_or(1) as usize),
            ('t', []) => match next_param_or(1) as usize {
                14 => handler.text_area_size_pixels(),
                18 => handler.text_area_size_chars(),
                22 => handler.push_title(),
                23 => handler.pop_title(),
                _ => unhandled!(),
            },
            ('u', [b'?']) => handler.report_keyboard_mode(),
            ('u', [b'=']) => {
                let mode = KeyboardModes::from_bits_truncate(next_param_or(0) as u8);
                let behavior = match next_param_or(1) {
                    3 => KeyboardModesApplyBehavior::Difference,
                    2 => KeyboardModesApplyBehavior::Union,
                    // Default is replace.
                    _ => KeyboardModesApplyBehavior::Replace,
                };
                handler.set_keyboard_mode(mode, behavior);
            },
            ('u', [b'>']) => {
                let mode = KeyboardModes::from_bits_truncate(next_param_or(0) as u8);
                handler.push_keyboard_mode(mode);
            },
            ('u', [b'<']) => {
                // The default is 1.
                handler.pop_keyboard_modes(next_param_or(1));
            },
            ('u', []) => handler.restore_cursor_position(),
            ('X', []) => handler.erase_chars(next_param_or(1) as usize),
            ('Z', []) => handler.move_backward_tabs(next_param_or(1)),
            _ => unhandled!(),
        }
    }

    #[inline]
    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        macro_rules! unhandled {
            () => {{
                debug!(
                    "[unhandled] esc_dispatch ints={:?}, byte={:?} ({:02x})",
                    intermediates, byte as char, byte
                );
            }};
        }

        macro_rules! configure_charset {
            ($charset:path, $intermediates:expr) => {{
                let index: CharsetIndex = match $intermediates {
                    [b'('] => CharsetIndex::G0,
                    [b')'] => CharsetIndex::G1,
                    [b'*'] => CharsetIndex::G2,
                    [b'+'] => CharsetIndex::G3,
                    _ => {
                        unhandled!();
                        return;
                    },
                };
                self.handler.configure_charset(index, $charset)
            }};
        }

        match (byte, intermediates) {
            (b'B', intermediates) => configure_charset!(StandardCharset::Ascii, intermediates),
            (b'D', []) => self.handler.linefeed(),
            (b'E', []) => {
                self.handler.linefeed();
                self.handler.carriage_return();
            },
            (b'H', []) => self.handler.set_horizontal_tabstop(),
            (b'M', []) => self.handler.reverse_index(),
            (b'Z', []) => self.handler.identify_terminal(None),
            (b'c', []) => self.handler.reset_state(),
            (b'0', intermediates) => {
                configure_charset!(StandardCharset::SpecialCharacterAndLineDrawing, intermediates)
            },
            (b'7', []) => self.handler.save_cursor_position(),
            (b'8', [b'#']) => self.handler.decaln(),
            (b'8', []) => self.handler.restore_cursor_position(),
            (b'=', []) => self.handler.set_keypad_application_mode(),
            (b'>', []) => self.handler.unset_keypad_application_mode(),
            // String terminator, do nothing (parser handles as string terminator).
            (b'\\', []) => (),
            _ => unhandled!(),
        }
    }

    #[inline]
    fn terminated(&self) -> bool {
        (self.sync_stops && self.terminated) || (self.halting && self.handler.halt_requested())
    }
}

#[inline]
fn attrs_from_sgr_parameters<H: Handler>(handler: &mut H, params: &mut ParamsIter<'_>) {
    while let Some(param) = params.next() {
        let attr = match param {
            [0] => Some(Attr::Reset),
            [1] => Some(Attr::Bold),
            [2] => Some(Attr::Dim),
            [3] => Some(Attr::Italic),
            [4, 0] => Some(Attr::CancelUnderline),
            [4, 2] => Some(Attr::DoubleUnderline),
            [4, 3] => Some(Attr::Undercurl),
            [4, 4] => Some(Attr::DottedUnderline),
            [4, 5] => Some(Attr::DashedUnderline),
            [4, ..] => Some(Attr::Underline),
            [5] => Some(Attr::BlinkSlow),
            [6] => Some(Attr::BlinkFast),
            [7] => Some(Attr::Reverse),
            [8] => Some(Attr::Hidden),
            [9] => Some(Attr::Strike),
            [21] => Some(Attr::CancelBold),
            [22] => Some(Attr::CancelBoldDim),
            [23] => Some(Attr::CancelItalic),
            [24] => Some(Attr::CancelUnderline),
            [25] => Some(Attr::CancelBlink),
            [27] => Some(Attr::CancelReverse),
            [28] => Some(Attr::CancelHidden),
            [29] => Some(Attr::CancelStrike),
            [30] => Some(Attr::Foreground(Color::Named(NamedColor::Black))),
            [31] => Some(Attr::Foreground(Color::Named(NamedColor::Red))),
            [32] => Some(Attr::Foreground(Color::Named(NamedColor::Green))),
            [33] => Some(Attr::Foreground(Color::Named(NamedColor::Yellow))),
            [34] => Some(Attr::Foreground(Color::Named(NamedColor::Blue))),
            [35] => Some(Attr::Foreground(Color::Named(NamedColor::Magenta))),
            [36] => Some(Attr::Foreground(Color::Named(NamedColor::Cyan))),
            [37] => Some(Attr::Foreground(Color::Named(NamedColor::White))),
            [38] => {
                let mut iter = params.map(|param| param[0]);
                parse_sgr_color(&mut iter).map(Attr::Foreground)
            },
            [38, params @ ..] => handle_colon_rgb(params).map(Attr::Foreground),
            [39] => Some(Attr::Foreground(Color::Named(NamedColor::Foreground))),
            [40] => Some(Attr::Background(Color::Named(NamedColor::Black))),
            [41] => Some(Attr::Background(Color::Named(NamedColor::Red))),
            [42] => Some(Attr::Background(Color::Named(NamedColor::Green))),
            [43] => Some(Attr::Background(Color::Named(NamedColor::Yellow))),
            [44] => Some(Attr::Background(Color::Named(NamedColor::Blue))),
            [45] => Some(Attr::Background(Color::Named(NamedColor::Magenta))),
            [46] => Some(Attr::Background(Color::Named(NamedColor::Cyan))),
            [47] => Some(Attr::Background(Color::Named(NamedColor::White))),
            [48] => {
                let mut iter = params.map(|param| param[0]);
                parse_sgr_color(&mut iter).map(Attr::Background)
            },
            [48, params @ ..] => handle_colon_rgb(params).map(Attr::Background),
            [49] => Some(Attr::Background(Color::Named(NamedColor::Background))),
            [58] => {
                let mut iter = params.map(|param| param[0]);
                parse_sgr_color(&mut iter).map(|color| Attr::UnderlineColor(Some(color)))
            },
            [58, params @ ..] => {
                handle_colon_rgb(params).map(|color| Attr::UnderlineColor(Some(color)))
            },
            [59] => Some(Attr::UnderlineColor(None)),
            [90] => Some(Attr::Foreground(Color::Named(NamedColor::BrightBlack))),
            [91] => Some(Attr::Foreground(Color::Named(NamedColor::BrightRed))),
            [92] => Some(Attr::Foreground(Color::Named(NamedColor::BrightGreen))),
            [93] => Some(Attr::Foreground(Color::Named(NamedColor::BrightYellow))),
            [94] => Some(Attr::Foreground(Color::Named(NamedColor::BrightBlue))),
            [95] => Some(Attr::Foreground(Color::Named(NamedColor::BrightMagenta))),
            [96] => Some(Attr::Foreground(Color::Named(NamedColor::BrightCyan))),
            [97] => Some(Attr::Foreground(Color::Named(NamedColor::BrightWhite))),
            [100] => Some(Attr::Background(Color::Named(NamedColor::BrightBlack))),
            [101] => Some(Attr::Background(Color::Named(NamedColor::BrightRed))),
            [102] => Some(Attr::Background(Color::Named(NamedColor::BrightGreen))),
            [103] => Some(Attr::Background(Color::Named(NamedColor::BrightYellow))),
            [104] => Some(Attr::Background(Color::Named(NamedColor::BrightBlue))),
            [105] => Some(Attr::Background(Color::Named(NamedColor::BrightMagenta))),
            [106] => Some(Attr::Background(Color::Named(NamedColor::BrightCyan))),
            [107] => Some(Attr::Background(Color::Named(NamedColor::BrightWhite))),
            _ => None,
        };

        match attr {
            Some(attr) => handler.terminal_attribute(attr),
            None => continue,
        }
    }
}

/// Handle colon separated rgb color escape sequence.
#[inline]
fn handle_colon_rgb(params: &[u16]) -> Option<Color> {
    let rgb_start = if params.len() > 4 { 2 } else { 1 };
    let rgb_iter = params[rgb_start..].iter().copied();
    let mut iter = iter::once(params[0]).chain(rgb_iter);

    parse_sgr_color(&mut iter)
}

/// Parse a color specifier from list of attributes.
fn parse_sgr_color(params: &mut dyn Iterator<Item = u16>) -> Option<Color> {
    match params.next() {
        Some(2) => Some(Color::Spec(Rgb {
            r: u8::try_from(params.next()?).ok()?,
            g: u8::try_from(params.next()?).ok()?,
            b: u8::try_from(params.next()?).ok()?,
        })),
        Some(5) => Some(Color::Indexed(u8::try_from(params.next()?).ok()?)),
        _ => None,
    }
}

/// C0 set of 7-bit control characters (from ANSI X3.4-1977).
#[allow(non_snake_case)]
pub mod C0 {
    /// Null filler, terminal should ignore this character.
    pub const NUL: u8 = 0x00;
    /// Start of Header.
    pub const SOH: u8 = 0x01;
    /// Start of Text, implied end of header.
    pub const STX: u8 = 0x02;
    /// End of Text, causes some terminal to respond with ACK or NAK.
    pub const ETX: u8 = 0x03;
    /// End of Transmission.
    pub const EOT: u8 = 0x04;
    /// Enquiry, causes terminal to send ANSWER-BACK ID.
    pub const ENQ: u8 = 0x05;
    /// Acknowledge, usually sent by terminal in response to ETX.
    pub const ACK: u8 = 0x06;
    /// Bell, triggers the bell, buzzer, or beeper on the terminal.
    pub const BEL: u8 = 0x07;
    /// Backspace, can be used to define overstruck characters.
    pub const BS: u8 = 0x08;
    /// Horizontal Tabulation, move to next predetermined position.
    pub const HT: u8 = 0x09;
    /// Linefeed, move to same position on next line (see also NL).
    pub const LF: u8 = 0x0A;
    /// Vertical Tabulation, move to next predetermined line.
    pub const VT: u8 = 0x0B;
    /// Form Feed, move to next form or page.
    pub const FF: u8 = 0x0C;
    /// Carriage Return, move to first character of current line.
    pub const CR: u8 = 0x0D;
    /// Shift Out, switch to G1 (other half of character set).
    pub const SO: u8 = 0x0E;
    /// Shift In, switch to G0 (normal half of character set).
    pub const SI: u8 = 0x0F;
    /// Data Link Escape, interpret next control character specially.
    pub const DLE: u8 = 0x10;
    /// (DC1) Terminal is allowed to resume transmitting.
    pub const XON: u8 = 0x11;
    /// Device Control 2, causes ASR-33 to activate paper-tape reader.
    pub const DC2: u8 = 0x12;
    /// (DC2) Terminal must pause and refrain from transmitting.
    pub const XOFF: u8 = 0x13;
    /// Device Control 4, causes ASR-33 to deactivate paper-tape reader.
    pub const DC4: u8 = 0x14;
    /// Negative Acknowledge, used sometimes with ETX and ACK.
    pub const NAK: u8 = 0x15;
    /// Synchronous Idle, used to maintain timing in Sync communication.
    pub const SYN: u8 = 0x16;
    /// End of Transmission block.
    pub const ETB: u8 = 0x17;
    /// Cancel (makes VT100 abort current escape sequence if any).
    pub const CAN: u8 = 0x18;
    /// End of Medium.
    pub const EM: u8 = 0x19;
    /// Substitute (VT100 uses this to display parity errors).
    pub const SUB: u8 = 0x1A;
    /// Prefix to an escape sequence.
    pub const ESC: u8 = 0x1B;
    /// File Separator.
    pub const FS: u8 = 0x1C;
    /// Group Separator.
    pub const GS: u8 = 0x1D;
    /// Record Separator (sent by VT132 in block-transfer mode).
    pub const RS: u8 = 0x1E;
    /// Unit Separator.
    pub const US: u8 = 0x1F;
    /// Delete, should be ignored by terminal.
    pub const DEL: u8 = 0x7F;
}

// Tests for parsing escape sequences.
//
// Byte sequences used in these tests are recording of pty stdout.
#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    pub struct TestSyncHandler {
        is_sync: usize,
    }

    impl Timeout for TestSyncHandler {
        #[inline]
        fn set_timeout(&mut self, _: Duration) {
            self.is_sync += 1;
        }

        #[inline]
        fn clear_timeout(&mut self) {
            self.is_sync = 0;
        }

        #[inline]
        fn pending_timeout(&self) -> bool {
            self.is_sync != 0
        }
    }

    struct MockHandler {
        index: CharsetIndex,
        charset: StandardCharset,
        attr: Option<Attr>,
        identity_reported: bool,
        color: Option<Rgb>,
        reset_colors: Vec<usize>,
        cursor_icon: Option<CursorIcon>,
    }

    impl Handler for MockHandler {
        fn terminal_attribute(&mut self, attr: Attr) {
            self.attr = Some(attr);
        }

        fn configure_charset(&mut self, index: CharsetIndex, charset: StandardCharset) {
            self.index = index;
            self.charset = charset;
        }

        fn set_active_charset(&mut self, index: CharsetIndex) {
            self.index = index;
        }

        fn identify_terminal(&mut self, _intermediate: Option<char>) {
            self.identity_reported = true;
        }

        fn reset_state(&mut self) {
            *self = Self::default();
        }

        fn set_color(&mut self, _: usize, c: Rgb) {
            self.color = Some(c);
        }

        fn reset_color(&mut self, index: usize) {
            self.reset_colors.push(index)
        }

        fn set_mouse_cursor_icon(&mut self, cursor_icon: CursorIcon) {
            self.cursor_icon = Some(cursor_icon);
        }
    }

    impl Default for MockHandler {
        fn default() -> MockHandler {
            MockHandler {
                index: CharsetIndex::G0,
                charset: StandardCharset::Ascii,
                attr: None,
                identity_reported: false,
                color: None,
                reset_colors: Vec::new(),
                cursor_icon: None,
            }
        }
    }

    /// The pointer shape requests (OSC 22) as they arrive, with their terminators.
    #[derive(Default)]
    struct PointerRequests(Vec<(Vec<u8>, String)>);

    impl Handler for PointerRequests {
        fn set_pointer_shape(&mut self, request: &[u8], terminator: &str) {
            self.0.push((request.to_vec(), terminator.to_owned()));
        }
    }

    #[test]
    fn pointer_shape_request_reaches_the_handler_as_written() {
        let mut parser = Processor::<TestSyncHandler>::new();
        let mut handler = PointerRequests::default();

        parser.advance(&mut handler, b"\x1b]22;\x1b\\\x1b]22;>pointer,text\x07\x1b]22\x07");

        let requests: Vec<_> =
            handler.0.iter().map(|(request, end)| (request.as_slice(), end.as_str())).collect();
        assert_eq!(requests, [
            (&b""[..], "\x1b\\"),
            (&b">pointer,text"[..], "\x07"),
            (&b""[..], "\x07"),
        ]);
    }

    #[test]
    fn pointer_shape_name_is_set_by_default() {
        let mut parser = Processor::<TestSyncHandler>::new();
        let mut handler = MockHandler::default();

        parser.advance(&mut handler, b"\x1b]22;pointer\x1b\\");
        assert_eq!(handler.cursor_icon, Some(CursorIcon::Pointer));

        // An empty request, or a list, is not one name: upstream's reading leaves it.
        parser.advance(&mut handler, b"\x1b]22;\x1b\\\x1b]22;>text\x1b\\");
        assert_eq!(handler.cursor_icon, Some(CursorIcon::Pointer));
    }

    #[test]
    fn parse_control_attribute() {
        static BYTES: &[u8] = &[0x1B, b'[', b'1', b'm'];

        let mut parser = Processor::<TestSyncHandler>::new();
        let mut handler = MockHandler::default();

        parser.advance(&mut handler, BYTES);

        assert_eq!(handler.attr, Some(Attr::Bold));
    }

    #[test]
    fn parse_terminal_identity_csi() {
        let bytes: &[u8] = &[0x1B, b'[', b'1', b'c'];

        let mut parser = Processor::<TestSyncHandler>::new();
        let mut handler = MockHandler::default();

        parser.advance(&mut handler, bytes);

        assert!(!handler.identity_reported);
        handler.reset_state();

        let bytes: &[u8] = &[0x1B, b'[', b'c'];

        parser.advance(&mut handler, bytes);

        assert!(handler.identity_reported);
        handler.reset_state();

        let bytes: &[u8] = &[0x1B, b'[', b'0', b'c'];

        parser.advance(&mut handler, bytes);

        assert!(handler.identity_reported);
    }

    #[test]
    fn parse_terminal_identity_esc() {
        let bytes: &[u8] = &[0x1B, b'Z'];

        let mut parser = Processor::<TestSyncHandler>::new();
        let mut handler = MockHandler::default();

        parser.advance(&mut handler, bytes);

        assert!(handler.identity_reported);
        handler.reset_state();

        let bytes: &[u8] = &[0x1B, b'#', b'Z'];

        let mut parser = Processor::<TestSyncHandler>::new();
        let mut handler = MockHandler::default();

        parser.advance(&mut handler, bytes);

        assert!(!handler.identity_reported);
        handler.reset_state();
    }

    #[test]
    fn parse_truecolor_attr() {
        static BYTES: &[u8] = &[
            0x1B, b'[', b'3', b'8', b';', b'2', b';', b'1', b'2', b'8', b';', b'6', b'6', b';',
            b'2', b'5', b'5', b'm',
        ];

        let mut parser = Processor::<TestSyncHandler>::new();
        let mut handler = MockHandler::default();

        parser.advance(&mut handler, BYTES);

        let spec = Rgb { r: 128, g: 66, b: 255 };

        assert_eq!(handler.attr, Some(Attr::Foreground(Color::Spec(spec))));
    }

    /// No exactly a test; useful for debugging.
    #[test]
    fn parse_zsh_startup() {
        static BYTES: &[u8] = &[
            0x1B, b'[', b'1', b'm', 0x1B, b'[', b'7', b'm', b'%', 0x1B, b'[', b'2', b'7', b'm',
            0x1B, b'[', b'1', b'm', 0x1B, b'[', b'0', b'm', b' ', b' ', b' ', b' ', b' ', b' ',
            b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ',
            b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ',
            b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ',
            b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ',
            b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ',
            b' ', b' ', b' ', b'\r', b' ', b'\r', b'\r', 0x1B, b'[', b'0', b'm', 0x1B, b'[', b'2',
            b'7', b'm', 0x1B, b'[', b'2', b'4', b'm', 0x1B, b'[', b'J', b'j', b'w', b'i', b'l',
            b'm', b'@', b'j', b'w', b'i', b'l', b'm', b'-', b'd', b'e', b's', b'k', b' ', 0x1B,
            b'[', b'0', b'1', b';', b'3', b'2', b'm', 0xE2, 0x9E, 0x9C, b' ', 0x1B, b'[', b'0',
            b'1', b';', b'3', b'2', b'm', b' ', 0x1B, b'[', b'3', b'6', b'm', b'~', b'/', b'c',
            b'o', b'd', b'e',
        ];

        let mut handler = MockHandler::default();
        let mut parser = Processor::<TestSyncHandler>::new();

        parser.advance(&mut handler, BYTES);
    }

    #[test]
    fn parse_designate_g0_as_line_drawing() {
        static BYTES: &[u8] = &[0x1B, b'(', b'0'];
        let mut parser = Processor::<TestSyncHandler>::new();
        let mut handler = MockHandler::default();

        parser.advance(&mut handler, BYTES);

        assert_eq!(handler.index, CharsetIndex::G0);
        assert_eq!(handler.charset, StandardCharset::SpecialCharacterAndLineDrawing);
    }

    #[test]
    fn parse_designate_g1_as_line_drawing_and_invoke() {
        static BYTES: &[u8] = &[0x1B, b')', b'0', 0x0E];
        let mut parser = Processor::<TestSyncHandler>::new();
        let mut handler = MockHandler::default();

        parser.advance(&mut handler, &BYTES[..3]);

        assert_eq!(handler.index, CharsetIndex::G1);
        assert_eq!(handler.charset, StandardCharset::SpecialCharacterAndLineDrawing);

        let mut handler = MockHandler::default();
        parser.advance(&mut handler, &[BYTES[3]]);

        assert_eq!(handler.index, CharsetIndex::G1);
    }

    #[test]
    fn parse_valid_rgb_colors() {
        assert_eq!(xparse_color(b"rgb:f/e/d"), Some(Rgb { r: 0xFF, g: 0xEE, b: 0xDD }));
        assert_eq!(xparse_color(b"rgb:11/aa/ff"), Some(Rgb { r: 0x11, g: 0xAA, b: 0xFF }));
        assert_eq!(xparse_color(b"rgb:f/ed1/cb23"), Some(Rgb { r: 0xFF, g: 0xEC, b: 0xCA }));
        assert_eq!(xparse_color(b"rgb:ffff/0/0"), Some(Rgb { r: 0xFF, g: 0x0, b: 0x0 }));
    }

    #[test]
    fn parse_valid_legacy_rgb_colors() {
        assert_eq!(xparse_color(b"#1af"), Some(Rgb { r: 0x10, g: 0xA0, b: 0xF0 }));
        assert_eq!(xparse_color(b"#11aaff"), Some(Rgb { r: 0x11, g: 0xAA, b: 0xFF }));
        assert_eq!(xparse_color(b"#110aa0ff0"), Some(Rgb { r: 0x11, g: 0xAA, b: 0xFF }));
        assert_eq!(xparse_color(b"#1100aa00ff00"), Some(Rgb { r: 0x11, g: 0xAA, b: 0xFF }));
    }

    #[test]
    fn parse_invalid_rgb_colors() {
        assert_eq!(xparse_color(b"rgb:0//"), None);
        assert_eq!(xparse_color(b"rgb://///"), None);
    }

    #[test]
    fn parse_invalid_legacy_rgb_colors() {
        assert_eq!(xparse_color(b"#"), None);
        assert_eq!(xparse_color(b"#f"), None);
    }

    #[test]
    fn parse_invalid_number() {
        assert_eq!(parse_number(b"1abc"), None);
    }

    #[test]
    fn parse_valid_number() {
        assert_eq!(parse_number(b"123"), Some(123));
    }

    #[test]
    fn parse_number_too_large() {
        assert_eq!(parse_number(b"321"), None);
    }

    #[test]
    fn parse_osc4_set_color() {
        let bytes: &[u8] = b"\x1b]4;0;#fff\x1b\\";

        let mut parser = Processor::<TestSyncHandler>::new();
        let mut handler = MockHandler::default();

        parser.advance(&mut handler, bytes);

        assert_eq!(handler.color, Some(Rgb { r: 0xF0, g: 0xF0, b: 0xF0 }));
    }

    #[test]
    fn parse_osc104_reset_color() {
        let bytes: &[u8] = b"\x1b]104;1;\x1b\\";

        let mut parser = Processor::<TestSyncHandler>::new();
        let mut handler = MockHandler::default();

        parser.advance(&mut handler, bytes);

        assert_eq!(handler.reset_colors, vec![1]);
    }

    #[test]
    fn parse_osc104_reset_all_colors() {
        let bytes: &[u8] = b"\x1b]104;\x1b\\";

        let mut parser = Processor::<TestSyncHandler>::new();
        let mut handler = MockHandler::default();

        parser.advance(&mut handler, bytes);

        let expected: Vec<usize> = (0..256).collect();
        assert_eq!(handler.reset_colors, expected);
    }

    #[test]
    fn parse_osc104_reset_all_colors_no_semicolon() {
        let bytes: &[u8] = b"\x1b]104\x1b\\";

        let mut parser = Processor::<TestSyncHandler>::new();
        let mut handler = MockHandler::default();

        parser.advance(&mut handler, bytes);

        let expected: Vec<usize> = (0..256).collect();
        assert_eq!(handler.reset_colors, expected);
    }

    #[test]
    fn partial_sync_updates() {
        let mut parser = Processor::<TestSyncHandler>::new();
        let mut handler = MockHandler::default();

        assert_eq!(parser.state.sync_state.timeout.is_sync, 0);
        assert!(handler.attr.is_none());

        // Start synchronized update.

        parser.advance(&mut handler, b"\x1b[?20");
        assert_eq!(parser.state.sync_state.timeout.is_sync, 0);
        assert!(handler.attr.is_none());

        parser.advance(&mut handler, b"26h");
        assert_eq!(parser.state.sync_state.timeout.is_sync, 1);
        assert!(handler.attr.is_none());

        // Dispatch some data.

        parser.advance(&mut handler, b"random \x1b[31m stuff");
        assert_eq!(parser.state.sync_state.timeout.is_sync, 1);
        assert!(handler.attr.is_none());

        // Extend synchronized update.

        parser.advance(&mut handler, b"\x1b[?20");
        assert_eq!(parser.state.sync_state.timeout.is_sync, 1);
        assert!(handler.attr.is_none());

        parser.advance(&mut handler, b"26h");
        assert_eq!(parser.state.sync_state.timeout.is_sync, 2);
        assert!(handler.attr.is_none());

        // Terminate synchronized update.

        parser.advance(&mut handler, b"\x1b[?20");
        assert_eq!(parser.state.sync_state.timeout.is_sync, 2);
        assert!(handler.attr.is_none());

        parser.advance(&mut handler, b"26l");
        assert_eq!(parser.state.sync_state.timeout.is_sync, 0);
        assert!(handler.attr.is_some());
    }

    #[test]
    fn sync_bursts_buffer() {
        let mut parser = Processor::<TestSyncHandler>::new();
        let mut handler = MockHandler::default();

        assert_eq!(parser.state.sync_state.timeout.is_sync, 0);
        assert!(handler.attr.is_none());

        // Repeat test twice to ensure internal state is reset properly.
        for _ in 0..2 {
            // Start synchronized update.
            parser.advance(&mut handler, b"\x1b[?2026h");
            assert_eq!(parser.state.sync_state.timeout.is_sync, 1);
            assert!(handler.attr.is_none());

            // Ensure sync works.
            parser.advance(&mut handler, b"\x1b[31m");
            assert_eq!(parser.state.sync_state.timeout.is_sync, 1);
            assert!(handler.attr.is_none());

            // Exceed sync buffer dimensions.
            parser.advance(&mut handler, "a".repeat(SYNC_BUFFER_SIZE).as_bytes());
            assert_eq!(parser.state.sync_state.timeout.is_sync, 0);
            assert!(handler.attr.take().is_some());

            // Ensure new events are dispatched directly.
            parser.advance(&mut handler, b"\x1b[31m");
            assert_eq!(parser.state.sync_state.timeout.is_sync, 0);
            assert!(handler.attr.take().is_some());
        }
    }

    #[test]
    fn mixed_sync_escape() {
        let mut parser = Processor::<TestSyncHandler>::new();
        let mut handler = MockHandler::default();

        assert_eq!(parser.state.sync_state.timeout.is_sync, 0);
        assert!(handler.attr.is_none());

        // Start synchronized update with immediate SGR.
        parser.advance(&mut handler, b"\x1b[?2026h\x1b[31m");
        assert_eq!(parser.state.sync_state.timeout.is_sync, 1);
        assert!(handler.attr.is_none());

        // Terminate synchronized update and check for SGR.
        parser.advance(&mut handler, b"\x1b[?2026l");
        assert_eq!(parser.state.sync_state.timeout.is_sync, 0);
        assert!(handler.attr.is_some());
    }

    #[test]
    fn sync_bsu_with_esu() {
        let mut parser = Processor::<TestSyncHandler>::new();
        let mut handler = MockHandler::default();

        assert_eq!(parser.state.sync_state.timeout.is_sync, 0);
        assert!(handler.attr.is_none());

        // Start synchronized update with immediate SGR.
        parser.advance(&mut handler, b"\x1b[?2026h\x1b[1m");
        assert_eq!(parser.state.sync_state.timeout.is_sync, 1);
        assert!(handler.attr.is_none());

        // Terminate synchronized update, but immediately start a new one.
        parser.advance(&mut handler, b"\x1b[?2026l\x1b[?2026h\x1b[4m");
        assert_eq!(parser.state.sync_state.timeout.is_sync, 2);
        assert_eq!(handler.attr.take(), Some(Attr::Bold));

        // Terminate again, expecting one buffered SGR.
        parser.advance(&mut handler, b"\x1b[?2026l");
        assert_eq!(parser.state.sync_state.timeout.is_sync, 0);
        assert_eq!(handler.attr.take(), Some(Attr::Underline));
    }

    /// What a handler was told, in order. Text is merged so a long run is one entry.
    #[derive(Debug, PartialEq, Eq, Clone)]
    enum Event {
        Text(String),
        Apc(Vec<u8>),
        Cancelled(Vec<u8>),
        Mode(bool, u16),
        Resolved,
    }

    /// A handler that asks to be halted after every complete application program command,
    /// the way one with work to do away from the caller's lock would.
    #[derive(Default)]
    struct Recorder {
        events: Vec<Event>,
        body: Option<Vec<u8>>,
        halt: bool,
        halts: usize,
    }

    impl Recorder {
        /// The events without the marks of the caller's pauses.
        fn told(&self) -> Vec<Event> {
            self.events.iter().filter(|event| **event != Event::Resolved).cloned().collect()
        }

        fn resolve(&mut self) {
            assert!(self.halt);
            self.halt = false;
            self.halts += 1;
            self.events.push(Event::Resolved);
        }
    }

    impl Handler for Recorder {
        fn input(&mut self, c: char) {
            match self.events.last_mut() {
                Some(Event::Text(text)) => text.push(c),
                _ => self.events.push(Event::Text(c.to_string())),
            }
        }

        fn set_private_mode(&mut self, mode: PrivateMode) {
            self.events.push(Event::Mode(true, mode.raw()));
        }

        fn unset_private_mode(&mut self, mode: PrivateMode) {
            self.events.push(Event::Mode(false, mode.raw()));
        }

        fn apc_start(&mut self) {
            assert!(self.body.replace(Vec::new()).is_none());
        }

        fn apc_put(&mut self, data: &[u8]) {
            assert!(!data.is_empty());
            self.body.as_mut().unwrap().extend_from_slice(data);
        }

        fn apc_end(&mut self, cancelled: bool) {
            let body = self.body.take().unwrap();
            if cancelled {
                self.events.push(Event::Cancelled(body));
            } else {
                self.events.push(Event::Apc(body));
                self.halt = true;
            }
        }

        fn halt_requested(&self) -> bool {
            self.halt
        }
    }

    /// `bytes` through `advance_until_halt` the way a caller must: whenever the handler
    /// halted, resolve and call again, with an empty slice if nothing is left.
    fn drive(
        processor: &mut Processor<TestSyncHandler>,
        handler: &mut Recorder,
        mut bytes: &[u8],
    ) {
        for _ in 0..1000 {
            let used = processor.advance_until_halt(handler, bytes);
            bytes = &bytes[used..];
            if handler.halt {
                handler.resolve();
            } else if bytes.is_empty() {
                return;
            } else {
                panic!("stopped with {} bytes left and nothing asked for it", bytes.len());
            }
        }
        panic!("no progress");
    }

    /// `input` in the reads `cuts` marks, halting at every command.
    fn halting_in_reads(input: &[u8], cuts: &[usize]) -> Recorder {
        let mut processor = Processor::<TestSyncHandler>::new();
        let mut handler = Recorder::default();
        let mut start = 0;
        for end in cuts.iter().copied().chain(iter::once(input.len())) {
            drive(&mut processor, &mut handler, &input[start..end]);
            start = end;
        }
        handler
    }

    /// The text and commands of `events` in order. Where in a synchronized update's bytes the
    /// modes are reported depends on the reads (a read that ends with the end of an update
    /// ends it, one that goes on is replayed along with the rest), so they are left out.
    fn commands_and_text(events: &[Event]) -> Vec<Event> {
        let mut kept: Vec<Event> = Vec::new();
        for event in events {
            match (kept.last_mut(), event) {
                (_, Event::Mode(..) | Event::Resolved) => (),
                (Some(Event::Text(text)), Event::Text(more)) => text.push_str(more),
                (_, event) => kept.push(event.clone()),
            }
        }
        kept
    }

    fn told_by_advance(input: &[u8]) -> Vec<Event> {
        let mut processor = Processor::<TestSyncHandler>::new();
        let mut handler = Recorder::default();
        processor.advance(&mut handler, input);
        commands_and_text(&handler.events)
    }

    /// Whatever the reads, a halting caller is told what `advance` tells, and every command
    /// is followed by exactly one pause before anything that came after it.
    fn assert_same_in_every_read(input: &[u8]) {
        let whole = told_by_advance(input);
        let commands = whole.iter().filter(|event| matches!(event, Event::Apc(_))).count();

        let check = |cuts: &[usize]| {
            let handler = halting_in_reads(input, cuts);
            assert_eq!(commands_and_text(&handler.events), whole, "cut at {cuts:?}");
            assert_eq!(handler.halts, commands, "cut at {cuts:?}");
            for (at, event) in handler.events.iter().enumerate() {
                if let Event::Apc(_) = event {
                    assert_eq!(
                        handler.events.get(at + 1),
                        Some(&Event::Resolved),
                        "cut at {cuts:?}"
                    );
                }
            }
        };
        for first in 0..=input.len() {
            for second in first..=input.len() {
                check(&[first, second]);
            }
        }
    }

    #[test]
    fn apc_reaches_the_handler() {
        let mut handler = Recorder::default();
        Processor::<TestSyncHandler>::new().advance(&mut handler, b"a\x1b_Gi=1;AAAA\x1b\\b");
        assert_eq!(handler.told(), [
            Event::Text("a".into()),
            Event::Apc(b"Gi=1;AAAA".to_vec()),
            Event::Text("b".into()),
        ]);
    }

    #[test]
    fn a_halt_returns_right_after_the_command() {
        let mut processor = Processor::<TestSyncHandler>::new();
        let mut handler = Recorder::default();
        let input = b"ab\x1b_Gx\x1b\\cd";

        // `\` is the 8th byte: the halt is after it, not at the `ESC` that began the end.
        assert_eq!(processor.advance_until_halt(&mut handler, input), 8);
        assert!(handler.halt);
        assert_eq!(handler.events, [Event::Text("ab".into()), Event::Apc(b"Gx".to_vec())]);

        // While it stays asked for, no byte is taken.
        assert_eq!(processor.advance_until_halt(&mut handler, &input[8..]), 0);
        assert_eq!(handler.events.len(), 2);

        handler.resolve();
        assert_eq!(processor.advance_until_halt(&mut handler, &input[8..]), 2);
        assert_eq!(handler.events[3], Event::Text("cd".into()));
    }

    #[test]
    fn a_standing_halt_takes_nothing_into_a_synchronized_update_either() {
        let mut processor = Processor::<TestSyncHandler>::new();
        let mut handler = Recorder::default();
        assert_eq!(processor.advance_until_halt(&mut handler, b"\x1b[?2026h"), 8);
        handler.halt = true;
        assert_eq!(processor.advance_until_halt(&mut handler, b"abc"), 0);
        assert_eq!(processor.sync_bytes_count(), 0);
        handler.halt = false;
        assert_eq!(processor.advance_until_halt(&mut handler, b"abc"), 3);
        assert_eq!(processor.sync_bytes_count(), 3);
    }

    #[test]
    fn a_halt_on_the_last_byte_is_reported() {
        let mut processor = Processor::<TestSyncHandler>::new();
        let mut handler = Recorder::default();
        let input = b"a\x1b_Gx\x1b\\";

        assert_eq!(processor.advance_until_halt(&mut handler, input), input.len());
        assert!(handler.halt);
    }

    #[test]
    fn advance_does_not_stop_for_a_halt() {
        let mut processor = Processor::<TestSyncHandler>::new();
        let mut handler = Recorder::default();
        processor.advance(&mut handler, b"\x1b_Gx\x1b\\ab\x1b_Gy\x1b\\cd");
        assert_eq!(handler.told(), [
            Event::Apc(b"Gx".to_vec()),
            Event::Text("ab".into()),
            Event::Apc(b"Gy".to_vec()),
            Event::Text("cd".into()),
        ]);
    }

    #[test]
    fn commands_and_halts_do_not_depend_on_the_reads() {
        // Text on both sides, a body with controls, `DEL`, UTF-8 and a cancelled command in
        // front of a complete one.
        assert_same_in_every_read(
            "ab\x1b_G\u{0}\u{7}\u{7f}é€;QQ\x1b\\\x1b[1mc\x1b_Gcut\x18d\x1b_Gx\x1b[0m\x1b_Gok\x1b\\e"
                .as_bytes(),
        );
    }

    #[test]
    fn commands_and_halts_inside_a_synchronized_update_wait_for_its_end() {
        let input = b"a\x1b[?2026hb\x1b_Gx\x1b\\c\x1b_Gy\x1b\\d\x1b[?2026le\x1b_Gz\x1b\\f";
        assert_same_in_every_read(input);

        // Nothing of the update reaches the handler before its end; then in order, with
        // a pause after each command.
        let mut processor = Processor::<TestSyncHandler>::new();
        let mut handler = Recorder::default();
        let end = input.windows(8).position(|window| window == ESU_CSI).unwrap();
        drive(&mut processor, &mut handler, &input[..end]);
        assert_eq!(handler.told(), [Event::Text("a".into()), Event::Mode(true, 2026)]);

        drive(&mut processor, &mut handler, &input[end..]);
        assert_eq!(handler.told(), [
            Event::Text("a".into()),
            Event::Mode(true, 2026),
            Event::Text("b".into()),
            Event::Apc(b"Gx".to_vec()),
            Event::Text("c".into()),
            Event::Apc(b"Gy".to_vec()),
            Event::Text("d".into()),
            Event::Mode(false, 2026),
            Event::Text("e".into()),
            Event::Apc(b"Gz".to_vec()),
            Event::Text("f".into()),
            Event::Mode(false, 2026),
        ]);
        assert_eq!(handler.halts, 3);
        assert_eq!(processor.sync_bytes_count(), 0);
    }

    #[test]
    fn a_halted_replay_keeps_the_update_that_follows_it() {
        // The first update ends and a second begins in the same bytes: the replay is of the
        // first and halts in it, the second must still be pending afterwards.
        let input = b"\x1b[?2026ha\x1b_Gx\x1b\\\x1b[?2026l\x1b[?2026hb\x1b_Gy\x1b\\";
        assert_same_in_every_read(input);

        let mut processor = Processor::<TestSyncHandler>::new();
        let mut handler = Recorder::default();
        drive(&mut processor, &mut handler, input);
        assert!(processor.state.sync_state.timeout.pending_timeout());
        assert!(processor.state.sync_state.replay.is_none());
        assert_eq!(handler.halts, 1);

        drive(&mut processor, &mut handler, b"\x1b[?2026l");
        assert!(!processor.state.sync_state.timeout.pending_timeout());
        assert_eq!(handler.halts, 2);
        assert_eq!(handler.told().last(), Some(&Event::Mode(false, 2026)));
    }

    #[test]
    fn a_full_sync_buffer_keeps_the_order_of_the_commands() {
        // The update is buffered with a command in it, then one read overflows the buffer: the
        // command is replayed first, and the halt in the replay leaves the read that
        // overflowed it unread until the replay is over.
        let first = b"\x1b[?2026hx\x1b_Gx\x1b\\y";
        let mut second = vec![b'a'; SYNC_BUFFER_SIZE];
        second.extend(b"\x1b_Gz\x1b\\z");

        let mut processor = Processor::<TestSyncHandler>::new();
        let mut handler = Recorder::default();
        drive(&mut processor, &mut handler, first);
        assert_eq!(handler.told(), [Event::Mode(true, 2026)]);
        drive(&mut processor, &mut handler, &second);

        let mut plain_processor = Processor::<TestSyncHandler>::new();
        let mut plain = Recorder::default();
        plain_processor.advance(&mut plain, first);
        plain_processor.advance(&mut plain, &second);

        assert_eq!(handler.told(), plain.told());
        assert_eq!(handler.told(), [
            Event::Mode(true, 2026),
            Event::Text("x".into()),
            Event::Apc(b"Gx".to_vec()),
            Event::Text("y".into()),
            Event::Mode(false, 2026),
            Event::Text("a".repeat(SYNC_BUFFER_SIZE)),
            Event::Apc(b"Gz".to_vec()),
            Event::Text("z".into()),
        ]);
        assert_eq!(handler.halts, 2);
        assert!(!processor.state.sync_state.timeout.pending_timeout());
    }

    #[test]
    fn a_timed_out_update_halts_and_ends_when_resolved() {
        let mut processor = Processor::<TestSyncHandler>::new();
        let mut handler = Recorder::default();

        drive(&mut processor, &mut handler, b"\x1b[?2026ha\x1b_Gx\x1b\\b");
        assert_eq!(handler.told(), [Event::Mode(true, 2026)]);

        assert!(processor.stop_sync_until_halt(&mut handler));
        assert_eq!(handler.told(), [
            Event::Mode(true, 2026),
            Event::Text("a".into()),
            Event::Apc(b"Gx".to_vec()),
        ]);
        assert!(processor.state.sync_state.timeout.pending_timeout());

        // Asking again with the request still open takes no step either.
        assert!(processor.stop_sync_until_halt(&mut handler));
        assert_eq!(handler.events.len(), 3);

        handler.resolve();
        drive(&mut processor, &mut handler, &[]);
        assert_eq!(handler.told().last(), Some(&Event::Mode(false, 2026)));
        assert_eq!(handler.told()[handler.told().len() - 2], Event::Text("b".into()));
        assert!(!processor.state.sync_state.timeout.pending_timeout());
        assert_eq!(processor.sync_bytes_count(), 0);
    }

    #[test]
    fn a_halt_on_the_last_byte_of_a_replay_ends_the_update_when_resolved() {
        let mut processor = Processor::<TestSyncHandler>::new();
        let mut handler = Recorder::default();

        drive(&mut processor, &mut handler, b"\x1b[?2026ha\x1b_Gx\x1b\\");
        assert_eq!(processor.sync_bytes_count(), 7);

        // The replay is at its end, but the update is not reported over before the pause.
        assert!(processor.stop_sync_until_halt(&mut handler));
        assert_eq!(handler.told(), [
            Event::Mode(true, 2026),
            Event::Text("a".into()),
            Event::Apc(b"Gx".to_vec()),
        ]);
        assert!(processor.state.sync_state.timeout.pending_timeout());

        handler.resolve();
        assert!(!processor.stop_sync_until_halt(&mut handler));
        assert_eq!(handler.told().last(), Some(&Event::Mode(false, 2026)));
        assert!(!processor.state.sync_state.timeout.pending_timeout());
        assert_eq!(processor.sync_bytes_count(), 0);
    }

    #[test]
    #[cfg(feature = "std")]
    fn contrast() {
        let rgb1 = Rgb { r: 0xFF, g: 0xFF, b: 0xFF };
        let rgb2 = Rgb { r: 0x00, g: 0x00, b: 0x00 };
        assert!((rgb1.contrast(rgb2) - 21.).abs() < f64::EPSILON);

        let rgb1 = Rgb { r: 0xFF, g: 0xFF, b: 0xFF };
        assert!((rgb1.contrast(rgb1) - 1.).abs() < f64::EPSILON);

        let rgb1 = Rgb { r: 0xFF, g: 0x00, b: 0xFF };
        let rgb2 = Rgb { r: 0x00, g: 0xFF, b: 0x00 };
        assert!((rgb1.contrast(rgb2) - 2.285_543_608_124_253_3).abs() < f64::EPSILON);

        let rgb1 = Rgb { r: 0x12, g: 0x34, b: 0x56 };
        let rgb2 = Rgb { r: 0xFE, g: 0xDC, b: 0xBA };
        assert!((rgb1.contrast(rgb2) - 9.786_558_997_257_74).abs() < f64::EPSILON);
    }
}
