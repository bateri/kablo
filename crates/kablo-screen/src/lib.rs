//! A terminal's screen as VT bytes.
//!
//! [`Screen`] keeps a screen from the bytes a program writes, with the same parser
//! (`alacritty_terminal` and its `vte`, the fork this repository carries) bateri uses, and
//! [`Screen::image`] writes it back as VT bytes that build the same screen in any other
//! terminal: feed the image to a fresh terminal of the same size, then the bytes that
//! follow, and its screen is the one the stream would have made. A client that missed more
//! output than anyone kept gets the picture and carries on from live output.
//!
//! **What an image carries**: both screens and which one is active; the scrollback (as
//! much as the screen was made to keep); every cell's characters (a base character and
//! the zero-width ones behind it, so a cluster travels as its bytes and the receiving
//! terminal clusters it as it likes), its colours — named, 256 and truecolour kept apart,
//! the underline colour too — and bold, dim, italic, inverse, hidden, strikeout and the
//! five underline styles; wide characters; which rows wrap into the next; OSC 8 links with
//! their ids; both cursors of each screen (position, a wrap waiting to happen, pen, open
//! link, character sets) and the cursor's shape and blinking; the scrolling region; the
//! tab stops; the active character set; the modes (cursor keys and keypad, the mouse modes
//! and encodings, bracketed paste, focus reports, wrap, insert, origin, newline, alternate
//! scroll, cursor visibility); the kitty keyboard stacks of both screens; the title and
//! the title stack; the palette changes (OSC 4, 10, 11, 12).
//!
//! **Two modes the emulator has no bit for travel too**: SGR-pixel mouse (`?1016`) and
//! in-band size reports (`?2048`) are dropped by `alacritty_terminal`, so the screen
//! follows the stream for them itself (`kept`) and the image turns on the ones that are on
//! (`CSI ? 1016 h`, `CSI ? 2048 h`), after everything else; a full reset (`ESC c`) turns them
//! off. The screen only remembers them: it sends no report of the size when `?2048` is
//! turned on, which is the terminal's owner to do.
//!
//! **What it does not**: kitty graphics (the parser drops those commands; an image is a
//! text screen); any other mode `alacritty_terminal` keeps no bit for; the selection and vi mode, which are the terminal's own; a title that was never set
//! after one that was comes back as the empty title; G2 or G3 as the active character set
//! comes back as G0 (no sequence invokes them). The order the image is written in, and the
//! limits that come from the writer driving the terminal, are in the header of `image.rs`.
//!
//! **A read can end inside a sequence** (or inside one character's UTF-8), and a client
//! that gets the image and then the bytes after that read would see the rest of a sequence
//! whose start it never had. The image therefore ends with the bytes of that open
//! sequence (the parser has acted on none of them yet) and the receiving parser picks the
//! sequence up where this one stands.
//!
//! **Synchronized updates are not held back**: a parser that buffers the bytes between
//! `?2026h` and `?2026l` would leave the screen behind the stream for up to a timeout, and
//! a screen that nothing wakes up cannot wait one out. The bytes are applied as they come;
//! the sequences themselves still go to a client in the live stream, where a terminal
//! that honours them does.
//!
//! **Reading a screen without writing it**: [`Screen::text`] is the text that is shown (the
//! scrollback above it when asked, wrapped rows joined when asked), [`Screen::modes`] the
//! modes the program set and [`Key::encode`] the bytes a key is in those modes; a key
//! is typed to a program the way a terminal would, whether or not one is attached.
//!
//! The terminal answers some questions by itself (device attributes, the cursor position,
//! mode reports); [`Screen::take_replies`] hands those bytes to whoever should write them
//! to the program. Questions that need the terminal's owner (colours, the clipboard, the
//! size in pixels) get no answer.

mod image;
mod kept;
mod keys;
mod tail;
mod text;

#[cfg(test)]
mod tests;

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{ClearMode, Handler, Processor, Timeout};

pub use keys::{Key, Modes, Mods};

use image::Size;
use tail::Tail;

/// How many bytes of the terminal's own answers wait to be taken; past that they are
/// dropped. A program that asks without reading the answers must not grow a screen that
/// nobody drains.
const REPLY_LIMIT: usize = 64 * 1024;

/// A parser timeout that never waits: synchronized updates are applied as they come (see
/// the crate header).
#[derive(Default)]
struct NoSync;

impl Timeout for NoSync {
    fn set_timeout(&mut self, _: Duration) {}

    fn clear_timeout(&mut self) {}

    fn pending_timeout(&self) -> bool {
        false
    }
}

/// What the terminal said through its listener: the title and the answers it gives.
#[derive(Default)]
struct Heard {
    title: Option<String>,
    replies: Vec<u8>,
}

/// The terminal's listener; the screen reads it back.
#[derive(Clone, Default)]
struct Recorder(Arc<Mutex<Heard>>);

impl Recorder {
    fn lock(&self) -> MutexGuard<'_, Heard> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn title(&self) -> Option<String> {
        self.lock().title.clone()
    }

    fn take_replies(&self) -> Vec<u8> {
        std::mem::take(&mut self.lock().replies)
    }
}

impl EventListener for Recorder {
    fn send_event(&self, event: Event) {
        let mut heard = self.lock();
        match event {
            Event::Title(title) => heard.title = Some(title),
            Event::ResetTitle => heard.title = None,
            Event::PtyWrite(text) => {
                let room = REPLY_LIMIT.saturating_sub(heard.replies.len());
                let take = text.len().min(room);
                if let Some(head) = text.as_bytes().get(..take) {
                    heard.replies.extend_from_slice(head);
                }
            }
            _ => {}
        }
    }
}

/// A screen kept from the bytes a program writes.
pub struct Screen {
    term: Term<Recorder>,
    parser: Processor<NoSync>,
    tail: Tail,
    heard: Recorder,
    history: usize,
    /// A forget that waits for the primary screen: a program holds the alternate one.
    forgetting: bool,
}

impl Screen {
    /// A blank screen of `columns` by `rows`, keeping `history` rows of scrollback. A
    /// size of zero is one.
    pub fn new(columns: u16, rows: u16, history: usize) -> Self {
        let heard = Recorder::default();
        Self {
            term: Self::term(&heard, usize::from(columns), usize::from(rows), history),
            parser: Processor::new(),
            tail: Tail::default(),
            heard,
            history,
            forgetting: false,
        }
    }

    /// The terminal itself. The kitty keyboard protocol is on: the screen follows what
    /// the stream says whether or not anyone asked about support, and an image carries
    /// the stacks.
    fn term(heard: &Recorder, columns: usize, rows: usize, history: usize) -> Term<Recorder> {
        let config = Config {
            scrolling_history: history,
            kitty_keyboard: true,
            ..Config::default()
        };
        let size = Size {
            columns: columns.max(1),
            lines: rows.max(1),
        };
        Term::new(config, &size, heard.clone())
    }

    /// Applies the bytes a program wrote, in the order it wrote them. Any split of the
    /// stream into calls gives the same screen.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.tail.feed(bytes);
        self.parser.advance(&mut self.term, bytes);
        if self.forgetting {
            self.forget_history();
        }
    }

    /// Forgets the scrollback: the rows that scrolled off the top of the screen. What is
    /// shown stays, and so does everything the screen keeps besides its rows; rows that
    /// scroll off later are kept as before.
    ///
    /// A program that holds the alternate screen (an editor, a pager) leaves the primary
    /// screen, and its scrollback, out of reach of the emulator until it gives the screen
    /// back; the forget waits for that and is done at the end of the first [`Screen::feed`]
    /// that finds the primary screen in use again, so rows that scrolled off in that same
    /// feed after the program left are forgotten too.
    pub fn forget_history(&mut self) {
        self.forgetting = self.term.mode().contains(TermMode::ALT_SCREEN);
        if !self.forgetting {
            self.term.clear_screen(ClearMode::Saved);
        }
    }

    /// Changes the size; rows reflow the way the terminal does it. A size of zero is one.
    pub fn resize(&mut self, columns: u16, rows: u16) {
        self.term.resize(Size {
            columns: usize::from(columns).max(1),
            lines: usize::from(rows).max(1),
        });
    }

    /// The size as `(columns, rows)`.
    pub fn size(&self) -> (u16, u16) {
        (
            u16::try_from(self.term.columns()).unwrap_or(u16::MAX),
            u16::try_from(self.term.screen_lines()).unwrap_or(u16::MAX),
        )
    }

    /// The title as the program last set it.
    pub fn title(&self) -> Option<String> {
        self.heard.title()
    }

    /// The bytes the terminal answered with since the last call: what a program that
    /// asked a question is waiting to read.
    pub fn take_replies(&mut self) -> Vec<u8> {
        self.heard.take_replies()
    }

    /// The text of the screen that is shown, one string per row with trailing spaces
    /// taken off: `history` rows of scrollback above it (as many as there are, no more
    /// than the screen keeps; the alternate screen has none), then every row of the
    /// screen, blank rows at the bottom included. With `unwrap`, the rows the terminal
    /// wrapped are one string again, so a line of 300 characters comes back as one on an
    /// 80-column screen; a space at the end of a wrapped row is the text's own and stays.
    /// A wide character is written once and a zero-width one behind its base.
    ///
    /// Only reads: neither the screen nor [`Screen::image`] is any different afterwards.
    pub fn text(&self, history: usize, unwrap: bool) -> Vec<String> {
        text::lines(self.term.grid(), history, unwrap)
    }

    /// The modes of the terminal that decide which bytes a key is: cursor keys, keypad
    /// and the kitty keyboard flags the program set. They are only what the program's own
    /// bytes made them; see [`Key::encode`].
    pub fn modes(&self) -> Modes {
        Modes::of(*self.term.mode())
    }

    /// The screen as VT bytes: a terminal of this size, fed the image and then the bytes
    /// [`Screen::feed`] gets next, shows what this screen will show. The image starts from
    /// a blank terminal (a client resets its own first) and ends with the open sequence
    /// the stream stopped in, if it did.
    ///
    /// The screen stays what it was and goes on: the writer has to take the terminal
    /// apart to read what it keeps private, so this one is built again from the image
    /// (which is also what it is checked against). Cost is that of parsing the screen and
    /// its history once.
    pub fn image(&mut self) -> Vec<u8> {
        let (columns, rows) = (self.term.columns(), self.term.screen_lines());
        let pending = self.heard.take_replies();
        let heard = self.heard.clone();
        let (mut image, _) = image::encode(&mut self.term, &move || heard.title());
        // The modes `Term` drops are the stream's, not the terminal's: written here, from
        // what the stream said, and read back by the `feed` of the image below.
        self.tail.kept().write(&mut image);
        image.extend_from_slice(self.tail.bytes());

        // The terminal is spent: a new one learns everything from the image, the title
        // included.
        self.heard.lock().title = None;
        self.term = Self::term(&self.heard, columns, rows, self.history);
        self.parser = Processor::new();
        self.tail = Tail::default();
        self.feed(&image);
        // The image holds no question, but whatever the rebuild said is not the
        // program's to hear; what was waiting before is.
        self.heard.take_replies();
        self.heard.lock().replies = pending;
        image
    }
}
