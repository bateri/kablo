//! The private modes `alacritty_terminal` keeps no bit for.
//!
//! `?1016` (SGR-pixel mouse: pointer reports in pixels) and `?2048` (in-band size reports)
//! reach `Term` as modes it does not know, and it drops them: nothing in the grid, the
//! mode set or the image writer's probes remembers that a program turned one on. An
//! application that draws the screen from an image, though, has to turn them on again, or a
//! program that asked for pixel reports gets cell reports from the moment it reconnects.
//! This follows the same bytes the parser does (see [`crate::tail::Tail`], which calls it at
//! the end of each CSI and at `ESC c`) and the image carries what it ends with.
//!
//! The rules are the parser's: a sequence is `CSI ?` followed by parameters and a final `h`
//! (on) or `l` (off); every parameter of the list counts, each as the number before a `:`;
//! numbers saturate at 65535; a sequence with more than 32 parameters (subparameters
//! included) or any byte that is not a digit, `;` or `:` after the `?` is ignored whole,
//! as `vte` and `alacritty_terminal` ignore it. Both modes are the session's, not a
//! screen's: the alternate screen has no modes of its own for these, as in bateri, and
//! only `ESC c` (a full reset) turns them off by itself.

/// The mode number of SGR-pixel mouse reports.
pub(crate) const PIXEL_MOUSE: u16 = 1016;

/// The mode number of in-band size reports.
pub(crate) const RESIZE_REPORTS: u16 = 2048;

/// The most separators (`;` and `:`) a sequence may have and still be dispatched: `vte`
/// keeps 32 parameter slots, one of them for the last parameter.
const MAX_SEPARATORS: usize = 31;

/// Which of the two modes a program turned on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Kept {
    pub(crate) pixel_mouse: bool,
    pub(crate) resize_reports: bool,
}

impl Kept {
    /// A CSI sequence ended with the byte `last`. `collected` is the sequence as far as
    /// the parser keeps it: `ESC [` and the parameter and intermediate bytes, without
    /// the final byte.
    pub(crate) fn csi(&mut self, collected: &[u8], last: u8) {
        let on = match last {
            b'h' => true,
            b'l' => false,
            _ => return,
        };
        let Some(params) = collected.strip_prefix(b"\x1b[?") else {
            return;
        };
        if !params
            .iter()
            .all(|byte| matches!(byte, b'0'..=b'9' | b';' | b':'))
        {
            return;
        }
        let separators = params
            .iter()
            .filter(|byte| matches!(byte, b';' | b':'))
            .count();
        if separators > MAX_SEPARATORS {
            return;
        }
        for group in params.split(|&byte| byte == b';') {
            // A subparameter says more of the same mode in the parser's eyes; only the
            // first number of a group names it.
            let number = group
                .split(|&byte| byte == b':')
                .next()
                .unwrap_or_default()
                .iter()
                .fold(0u16, |number, &digit| {
                    number
                        .saturating_mul(10)
                        .saturating_add(u16::from(digit - b'0'))
                });
            match number {
                PIXEL_MOUSE => self.pixel_mouse = on,
                RESIZE_REPORTS => self.resize_reports = on,
                _ => {}
            }
        }
    }

    /// `ESC c`: both off, as on a fresh terminal.
    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }

    /// The sequences that turn on what is on; a blank terminal has both off, so nothing
    /// is written for what is off.
    pub(crate) fn write(self, out: &mut Vec<u8>) {
        if self.pixel_mouse {
            out.extend_from_slice(b"\x1b[?1016h");
        }
        if self.resize_reports {
            out.extend_from_slice(b"\x1b[?2048h");
        }
    }
}
