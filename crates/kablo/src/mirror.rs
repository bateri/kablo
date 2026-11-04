//! The relay's screen, and the thread that parses it while someone is attached.
//!
//! The screen follows the journal: it has had every byte below `at`. Who gives it the
//! rest depends on whether anyone is attached. With nobody, the relay's own thread does
//! it right after each read (and answers what the program asks, the terminal's questions
//! being the relay's to hear). With a client attached, the client's terminal answers and
//! the screen is only a record, so a worker thread parses behind the relay's reads and
//! the relay's read-and-forward path never waits for the parser.
//!
//! **One lock holds all of it** ([`Core`]): the screen, how far it has been given, the
//! floor it has forgotten at and the end of the last stretch that held a question. Whoever
//! holds it feeds the screen in stream order, so any thread that wants the screen as it is
//! (a picture, a capture, a resize) takes the lock and first gives the screen what it has
//! not had.
//!
//! **The worker never goes past `answered`**, the journal's offset up to which the
//! terminal's questions were answered. The relay moves it to the end of what it read right
//! after each read that an attached client was shown, so the worker's work is exactly the
//! bytes whose questions are not its to answer; what it would parse beyond that, because a
//! read with nobody attached came after, is read by the relay's own thread with its
//! answers. That is also what keeps a question in a backlog from being answered by the
//! relay after the client that was shown it has gone.
//!
//! **No timer and no polling:** the worker waits on a condition variable when it has
//! caught up, and the relay notifies it only when it is parked (an atomic says so), so a
//! read costs no system call while the worker is running. An idle relay never wakes it.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering, fence};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};

use kablo_screen::Screen;

use crate::journal::Journal;

/// Whose questions the bytes given to the screen may hold.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Feed {
    /// Bytes this relay has just read: what the program asks in them is this
    /// relay's to answer, unless they are below the offset the journal says was
    /// answered (a client was shown them).
    Live,
    /// Bytes the journal held already: those after the offset it says it
    /// answered up to were read while no relay was (by the warden, in the gap
    /// before this relay or between two), and their questions are answered now;
    /// the others were answered by whoever read them.
    Held,
    /// Bytes nobody is to answer (the last output of a program that has ended).
    Spent,
    /// Bytes an attached client was shown: those below the offset answered, and
    /// no further. The worker's.
    Behind,
}

/// Bytes of the journal given to the screen in one piece.
pub(crate) const SCREEN_CHUNK: usize = 64 * 1024;

/// How much of the journal a relay that takes over reads to build its screen:
/// the last this many bytes. Replaying a journal of a gigabyte before the first
/// client is served would be a pause the swap is meant not to have, and what
/// the screen shows rarely depends on output that old.
pub(crate) const REPLAY_LIMIT: u64 = 64 << 20;

/// How many times the screen is begun again while a relay that is still
/// writing laps it. Past it the screen goes on as it is: it may be wrong, and
/// the next picture is the best there is.
pub(crate) const SCREEN_RESTARTS: u32 = 8;

/// How many times the worker steps aside for a thread that is waiting for the lock before it
/// takes the lock again regardless.
const MAKE_WAY: u32 = 64;

/// What one piece of feeding came to.
pub(crate) enum Piece {
    /// The screen has all it may have.
    Caught,
    /// The screen was given a piece. `replies` is what the terminal answered to the
    /// questions in it; `answers` says they are this relay's to send.
    Fed { replies: Vec<u8>, answers: bool },
    /// The ring was lapped under the screen (a relay that is still writing, while this one
    /// catches up); [`ScreenState::restart`] begins again from what is left.
    Lapped,
}

/// The screen and where it stands in the journal.
pub(crate) struct ScreenState {
    pub(crate) screen: Screen,
    /// The offset after the last journal byte the screen has had.
    pub(crate) at: u64,
    /// The journal's floor (where the past was last forgotten) that the screen has been
    /// brought to: it forgot its scrollback on reaching it, or began after it and had
    /// none from before to forget. A relay that starts builds its screen from what the ring
    /// still holds and forgets at the floor on the way, as the one before it did when it was
    /// asked.
    pub(crate) forgotten: u64,
    /// The end of the last stretch of output that held a question the terminal
    /// had an answer for, whoever sent it to the program (this relay with
    /// nobody attached, or a client's own terminal). A relay that takes over
    /// finds it again by feeding the screen the journal it holds, and counts
    /// what it cannot replay as answered: a picture costs less than a program
    /// answered twice.
    pub(crate) last_reply: u64,
    history: usize,
    /// Reused to carry journal bytes to the screen.
    scratch: Vec<u8>,
}

impl ScreenState {
    pub(crate) fn new(screen: Screen, at: u64, last_reply: u64, history: usize) -> Self {
        Self {
            screen,
            at,
            forgotten: 0,
            last_reply,
            history,
            scratch: vec![0; SCREEN_CHUNK],
        }
    }

    /// Gives the screen one piece of the journal it has not had.
    pub(crate) fn feed_piece(&mut self, journal: &Journal, mode: Feed) -> Piece {
        let total = journal.total();
        self.reach_floor(journal);
        if self.at >= total {
            return Piece::Caught;
        }
        let mut wanted = usize::try_from(total - self.at)
            .unwrap_or(usize::MAX)
            .min(self.scratch.len());
        // Not past the floor in one piece: the screen forgets its scrollback at it.
        let floor = journal.floor();
        if self.forgotten < floor {
            wanted = wanted.min(usize::try_from(floor - self.at).unwrap_or(wanted));
        }
        let answered = journal.answered();
        let answers = match (mode, answered) {
            (Feed::Spent, _) => false,
            (Feed::Live, None) => true,
            (Feed::Held, None) => false,
            (Feed::Behind, None) => return Piece::Caught,
            (Feed::Behind, Some(upto)) if self.at >= upto => return Piece::Caught,
            (_, Some(upto)) if self.at >= upto => true,
            // Up to where it was answered, and no further in one piece.
            (_, Some(upto)) => {
                wanted = wanted.min(usize::try_from(upto - self.at).unwrap_or(wanted));
                false
            }
        };
        let got = self
            .scratch
            .get_mut(..wanted)
            .and_then(|room| journal.copy_out(self.at, room));
        match got {
            Some(0) => Piece::Caught,
            Some(n) => {
                self.screen.feed(self.scratch.get(..n).unwrap_or_default());
                self.at += n as u64;
                let replies = self.screen.take_replies();
                if !replies.is_empty() && mode != Feed::Spent {
                    self.last_reply = self.last_reply.max(self.at);
                }
                if answers || mode == Feed::Spent {
                    // Never back: a piece of a longer stretch must not lower what an
                    // earlier piece of it raised.
                    journal.set_answered(self.at.max(answered.unwrap_or(0)));
                }
                Piece::Fed { replies, answers }
            }
            None => Piece::Lapped,
        }
    }

    /// Begins the screen again from what the ring still holds.
    pub(crate) fn restart(&mut self, journal: &Journal) {
        let (cols, rows) = self.screen.size();
        self.screen = Screen::new(cols, rows, self.history);
        self.at = replay_start(journal, REPLAY_LIMIT);
        self.forgotten = 0;
    }

    /// Has the screen forget its scrollback if it stands at the journal's floor, where
    /// the past was forgotten: the rows before are gone from what a client is given, and
    /// the picture of the screen must not bring them back. A screen that began after the
    /// floor has none from before it.
    pub(crate) fn reach_floor(&mut self, journal: &Journal) {
        let floor = journal.floor();
        if self.forgotten < floor && self.at >= floor {
            if self.at == floor {
                self.screen.forget_history();
            }
            self.forgotten = floor;
        }
    }

    /// Whether [`Feed::Behind`] has something to give the screen.
    fn behind(&self, journal: &Journal) -> bool {
        journal
            .answered()
            .is_some_and(|upto| self.at < upto && self.at < journal.total())
    }
}

/// What the relay's thread and the worker share.
pub(crate) struct Core {
    state: Mutex<ScreenState>,
    pub(crate) journal: Arc<Journal>,
    /// A copy of the state's `at`, for the relay to ask how far behind the screen is
    /// without the lock. It may be a little old; it is only a guide to when to catch up.
    parsed: AtomicU64,
    /// The worker waits on `wake` and says so, so that the relay notifies it only then.
    parked: AtomicBool,
    wake: Condvar,
    stop: AtomicBool,
    /// How many threads are waiting for the lock on the relay's behalf: the worker lets
    /// them in before it takes the lock again.
    waiting: AtomicUsize,
}

impl Core {
    pub(crate) fn new(state: ScreenState, journal: Arc<Journal>) -> Arc<Core> {
        Arc::new(Core {
            parsed: AtomicU64::new(state.at),
            state: Mutex::new(state),
            journal,
            parked: AtomicBool::new(false),
            wake: Condvar::new(),
            stop: AtomicBool::new(false),
            waiting: AtomicUsize::new(0),
        })
    }

    /// The lock, for the relay's thread. A poisoned lock (the worker panicked in the
    /// parser) is taken all the same: the relay must not panic on it, and the screen is
    /// whatever the parser left.
    pub(crate) fn lock(&self) -> MutexGuard<'_, ScreenState> {
        self.waiting.fetch_add(1, Ordering::AcqRel);
        let guard = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        self.waiting.fetch_sub(1, Ordering::AcqRel);
        guard
    }

    /// Records how far the screen has got.
    pub(crate) fn parsed_to(&self, at: u64) {
        self.parsed.store(at, Ordering::Release);
    }

    /// How far the screen was last known to have got.
    pub(crate) fn parsed(&self) -> u64 {
        self.parsed.load(Ordering::Acquire)
    }

    /// Wakes the worker if it is parked: the relay has moved `answered` on. The fence
    /// pairs with the worker's, so that either the worker sees the new offset before it
    /// waits or the relay sees that it is about to.
    pub(crate) fn wake_worker(&self) {
        fence(Ordering::SeqCst);
        if self.parked.load(Ordering::SeqCst) {
            // Under the lock: the worker holds it from the check until it waits.
            let _guard = self.lock();
            self.wake.notify_one();
        }
    }

    fn work(&self) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let mut lapped = 0;
        loop {
            if self.stop.load(Ordering::Acquire) {
                return;
            }
            match state.feed_piece(&self.journal, Feed::Behind) {
                Piece::Fed { .. } => {
                    // What the terminal said is the attached client's terminal's to hear.
                    lapped = 0;
                    self.parsed_to(state.at);
                    drop(state);
                    self.make_way();
                    state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
                }
                Piece::Lapped => {
                    lapped += 1;
                    if lapped > SCREEN_RESTARTS {
                        // The screen goes on as it is, and is brought up by the relay's
                        // own catching up.
                        state.at = self
                            .journal
                            .answered()
                            .unwrap_or(0)
                            .min(self.journal.total());
                        lapped = 0;
                    } else {
                        state.restart(&self.journal);
                    }
                    self.parsed_to(state.at);
                }
                Piece::Caught => {
                    self.parked.store(true, Ordering::SeqCst);
                    fence(Ordering::SeqCst);
                    if self.stop.load(Ordering::Acquire) {
                        return;
                    }
                    if !state.behind(&self.journal) {
                        state = self
                            .wake
                            .wait(state)
                            .unwrap_or_else(PoisonError::into_inner);
                    }
                    self.parked.store(false, Ordering::SeqCst);
                }
            }
        }
    }

    /// Lets a thread that is waiting for the lock have it: a mutex does not hand over,
    /// and a worker that took it back at once would keep the relay out for as long as
    /// it had work.
    fn make_way(&self) {
        let mut turns = 0;
        while self.waiting.load(Ordering::Acquire) > 0 && turns < MAKE_WAY {
            thread::yield_now();
            turns += 1;
        }
    }
}

/// The thread that parses behind the relay's reads, until it is dropped.
pub(crate) struct Worker {
    core: Arc<Core>,
    handle: Option<JoinHandle<()>>,
}

impl Worker {
    pub(crate) fn start(core: &Arc<Core>) -> std::io::Result<Worker> {
        let shared = Arc::clone(core);
        let handle = thread::Builder::new()
            .name("kablo-screen".to_owned())
            .spawn(move || {
                // A panic in the parser ends the relay, as it would on the relay's own
                // thread, and the warden starts another from the journal: a screen half
                // way through a piece is not one to go on with.
                if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| shared.work())).is_err()
                {
                    std::process::exit(101);
                }
            })?;
        Ok(Worker {
            core: Arc::clone(core),
            handle: Some(handle),
        })
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.core.stop.store(true, Ordering::Release);
        {
            let _guard = self.core.lock();
            self.core.wake.notify_all();
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Where `last_reply` starts for a relay that feeds its screen from
/// `replay_start` on: what it will not replay counts as answered, so does all
/// of a journal that keeps no track of answers (an older warden's), and the
/// replay moves the mark to the end of what held a question.
pub(crate) fn replayed_mark(journal: &Journal, replay_start: u64) -> u64 {
    match journal.answered() {
        Some(_) => replay_start,
        None => journal.total(),
    }
}

/// Where the screen of a journal begins: the start of the stream when nothing
/// was dropped and no more than `limit` bytes are there, else the first line
/// break in the last `limit` bytes that are still in the ring, so that the
/// screen does not start inside an escape sequence.
pub(crate) fn replay_start(journal: &Journal, limit: u64) -> u64 {
    let from = journal.total().saturating_sub(limit);
    if from == 0 && journal.ring_from() == 0 {
        0
    } else {
        journal.line_start_from(from)
    }
}
