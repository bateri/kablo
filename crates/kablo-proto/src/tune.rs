//! How large a stream's window is: as large as the link needs and no larger.
//!
//! A window has to hold what is on its way while the credit for it comes back,
//! or a transfer waits a round trip for every window. A window larger than
//! that does not make the transfer faster: the bytes over it wait in the
//! carrier (ssh's buffers, the network's queues) and so does everything sent
//! behind them on the same connection, typing and its echo among it. So the
//! end that gives a window measures what the link carries and gives that.
//!
//! **Two measures, both from the credit.** When an end gives credit, the other
//! end could not have sent a byte past what it was allowed before; the first
//! such byte arrives a round trip later, plus whatever waited in a queue on the
//! way. The smallest such time seen on the connection is the round trip
//! ([`Rtt`]); the bytes that arrived from one such sample to the next, over the
//! time between, are the rate the stream went at. The window is
//! [`GAIN`] times the highest rate of the last [`RATES`] samples times the round
//! trip: twice what the link carries in a round trip, because a network that
//! holds bytes back for a moment and then lets them through at once (Wi-Fi, a
//! tunnel) stalls a transfer whose window covers the round trip and no more.
//! While the window holds a transfer up, the rate it is measured at is the
//! window's own, so it doubles each round trip until the link is what holds it
//! up. On a link with a deep queue (a slow uplink, a tunnel) the window then
//! keeps a round trip's worth waiting in it, and no more: a window fixed at
//! 2 MiB kept seconds' worth on a link of 1 MB/s.
//!
//! Neither measure is moved by a network that jitters, which is why it is
//! these and not the delay itself: a late sample is not the least, and a slow
//! round trip is not the highest rate. A queue that the window itself built is
//! the one thing that would move the round trip, once the old one is let go:
//! so when no sample has come close to the round trip for [`RTT_KEPT`] (one
//! that does says it still is the round trip), the connection **drains**:
//! every stream's window is [`LEAST`] until a credit given then comes back as
//! bytes, and that time, with the queue gone, is the round trip from then on
//! (larger or smaller: a path that became slower is learned). That costs a
//! round trip or two of a transfer every ten seconds, and only where a queue
//! stood the whole time.
//!
//! **A path that became slower** (a round trip that grew for reasons of its
//! own) would make a window measured against the old round trip hold the
//! transfer to a fraction of the link, and the rate it then went at would
//! shrink the window further, down to nothing. So the window is also never
//! under the highest rate times the round trip as it is now, smoothed over
//! the samples ([`Rtt::now`]): that is what is on the way at that rate, once,
//! with nothing over it, so a queue of the window's own cannot make it grow
//! and a slower path cannot make it shrink. The next drain then learns the
//! slower round trip, and the window grows to it.
//!
//! Nothing here reads a clock: the times come from the caller, which takes them
//! when bytes arrive and credit leaves, so nothing wakes when nothing moves.

use std::time::{Duration, Instant};

/// The window a stream is given before its connection has measured the link.
pub const START: u32 = 256 * 1024;

/// The smallest window: two `data` messages, so that one can be on its way
/// while the other is taken. It is also every stream's window while the
/// connection drains.
pub const LEAST: u32 = 64 * 1024;

/// How long the round trip stands with no sample close to it before the
/// connection drains to see it again.
pub const RTT_KEPT: Duration = Duration::from_secs(10);

/// How close to the round trip a sample has to be to say it still is: a
/// sixteenth of it, and at least a millisecond.
fn close(rtt: Duration) -> Duration {
    rtt + (rtt / 16).max(Duration::from_millis(1))
}

/// The window over what the link carries in a round trip: twice it.
pub const GAIN: (u32, u32) = (2, 1);

/// How many rate samples the highest is taken from: about a second's worth.
pub const RATES: usize = 10;

/// The connection's round trip: the smallest sample, and whether the
/// connection is draining to take it again.
#[derive(Clone, Debug, Default)]
pub struct Rtt {
    /// The smallest sample, and when it or one close to it was taken.
    least: Option<(Duration, Instant)>,
    draining: bool,
    /// The samples, smoothed: seven eighths of what it was and an eighth of
    /// each new one.
    smoothed: Option<Duration>,
}

impl Rtt {
    /// No samples yet.
    pub fn new() -> Rtt {
        Rtt::default()
    }

    /// The round trip as measured so far.
    pub fn get(&self) -> Option<Duration> {
        self.least.map(|(least, _)| least)
    }

    /// The round trip as it is now: the samples smoothed, queue and all.
    pub fn now(&self) -> Option<Duration> {
        self.smoothed
    }

    /// Whether every stream's window is [`LEAST`] for now.
    pub fn draining(&self) -> bool {
        self.draining
    }

    /// Starts draining if the round trip has stood its time by `now`.
    pub fn look(&mut self, now: Instant) {
        if let Some((_, at)) = self.least
            && now.saturating_duration_since(at) > RTT_KEPT
        {
            self.draining = true;
        }
    }

    /// A sample taken at `now`; `drained` says whether its credit left while
    /// the connection drained, so that no queue of its own was in it.
    pub fn sample(&mut self, sample: Duration, now: Instant, drained: bool) {
        self.smoothed = Some(match self.smoothed {
            Some(smoothed) => smoothed * 7 / 8 + sample / 8,
            None => sample,
        });
        match self.least {
            Some((least, _)) if sample > least && !(self.draining && drained) => {
                if sample <= close(least) {
                    // Still the round trip: no queue stands.
                    self.least = Some((least, now));
                    self.draining = false;
                }
            }
            _ => {
                self.least = Some((sample, now));
                self.draining = false;
            }
        }
    }
}

/// The credit being timed.
#[derive(Clone, Copy, Debug)]
struct Probe {
    /// What the other end could have sent before it.
    limit: u64,
    at: Instant,
    /// It left while the connection drained.
    drained: bool,
}

/// One stream's window, as its receiving end measures it.
#[derive(Clone, Debug)]
pub struct Tuner {
    window: u32,
    probe: Option<Probe>,
    /// Bytes received in all at the last sample, and when.
    last: Option<(u64, Instant)>,
    /// The rates of the last samples, in bytes a second.
    rates: [u64; RATES],
    next: usize,
}

impl Tuner {
    /// A stream whose window starts at `window`.
    pub fn new(window: u32) -> Tuner {
        Tuner {
            window,
            probe: None,
            last: None,
            rates: [0; RATES],
            next: 0,
        }
    }

    /// The window the stream should have now, within `cap`: [`LEAST`] while
    /// the connection drains.
    pub fn window(&self, rtt: &Rtt, cap: u32) -> u32 {
        if rtt.draining() {
            LEAST.min(cap)
        } else {
            self.window.min(cap)
        }
    }

    /// The window as measured, drain or not: where the next stream on the
    /// connection starts.
    pub fn measured(&self) -> u32 {
        self.window
    }

    /// Credit leaves at `now`; the other end could have sent `limit` bytes in
    /// all before it. Timed if no other credit is.
    pub fn crediting(&mut self, limit: u64, now: Instant, rtt: &Rtt) {
        if self.probe.is_none() {
            self.probe = Some(Probe {
                limit,
                at: now,
                drained: rtt.draining(),
            });
        }
    }

    /// Bytes arrived at `now`, `received` in all. A byte past the limit of the
    /// credit being timed is a sample: of the round trip, into `rtt`, and of
    /// the rate since the last one; the window is set from them, within
    /// `cap`.
    pub fn received(&mut self, received: u64, now: Instant, rtt: &mut Rtt, cap: u32) {
        let Some(probe) = self.probe else {
            return;
        };
        if received <= probe.limit {
            return;
        }
        self.probe = None;
        rtt.sample(now.saturating_duration_since(probe.at), now, probe.drained);
        if let Some((before, then)) = self.last {
            let nanos = now.saturating_duration_since(then).as_nanos().max(1);
            let bytes = u128::from(received.saturating_sub(before));
            let rate = u64::try_from(bytes * 1_000_000_000 / nanos).unwrap_or(u64::MAX);
            if let Some(slot) = self.rates.get_mut(self.next) {
                *slot = rate;
            }
            self.next = (self.next + 1) % RATES;
        }
        self.last = Some((received, now));
        let highest = self.rates.iter().copied().max().unwrap_or(0);
        if let (Some(least), Some(now), true) = (rtt.get(), rtt.now(), highest > 0) {
            let carried =
                |round_trip: Duration| u128::from(highest) * round_trip.as_nanos() / 1_000_000_000;
            let (times, over) = GAIN;
            let window = (carried(least) * u128::from(times) / u128::from(over)).max(carried(now));
            self.window = u32::try_from(window).unwrap_or(u32::MAX);
        }
        let floor = LEAST.min(cap);
        self.window = self.window.clamp(floor, cap.max(floor));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAP: u32 = 4 << 20;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// A stream as the tuner sees it: each round the receiver gives credit,
    /// and the bytes of the round arrive, the last of them past its limit.
    struct Link {
        tuner: Tuner,
        rtt: Rtt,
        now: Instant,
        received: u64,
    }

    impl Link {
        fn new() -> Link {
            Link {
                tuner: Tuner::new(START),
                rtt: Rtt::new(),
                now: Instant::now(),
                received: 0,
            }
        }

        /// One round: `bytes` arrive over `took`.
        fn round(&mut self, bytes: u64, took: Duration) {
            self.tuner.crediting(self.received, self.now, &self.rtt);
            self.now += took;
            self.received += bytes;
            self.tuner
                .received(self.received, self.now, &mut self.rtt, CAP);
            self.rtt.look(self.now);
        }

        fn window(&self) -> u32 {
            self.tuner.window(&self.rtt, CAP)
        }
    }

    #[test]
    fn a_window_that_holds_the_transfer_up_doubles_each_round_trip() {
        let mut link = Link::new();
        // 70 ms, and a link far faster than the window: each round carries the
        // window.
        link.round(u64::from(link.window()), ms(70));
        assert_eq!(
            link.window(),
            START,
            "one sample is a round trip, not a rate"
        );
        let mut before = link.window();
        for _ in 0..3 {
            link.round(u64::from(link.window()), ms(70));
            let now = link.window();
            assert!(now.abs_diff(before * 2) < 16, "{before} grew to {now}");
            before = now;
        }
        for _ in 0..20 {
            link.round(u64::from(link.window()), ms(70));
        }
        assert_eq!(link.window(), CAP);
    }

    #[test]
    fn a_link_slower_than_the_window_gets_twice_what_it_carries_in_a_round_trip() {
        let mut link = Link::new();
        // 8 MB/s over 70 ms carries 560 KB a round trip; a queue makes some
        // rounds longer, which leaves the least round trip and the highest rate
        // as they were.
        for i in 0..30 {
            let took = if i % 3 == 0 { ms(130) } else { ms(70) };
            let bytes = 8_000_000 * u64::try_from(took.as_millis()).unwrap() / 1000;
            link.round(bytes, took);
        }
        assert_eq!(link.rtt.get(), Some(ms(70)));
        assert_eq!(link.window(), 1_120_000);
    }

    #[test]
    fn a_late_round_does_not_shrink_the_window() {
        let mut link = Link::new();
        for _ in 0..12 {
            link.round(560_000, ms(70));
        }
        let steady = link.window();
        assert_eq!(steady, 1_120_000);
        // A round that took 150 ms longer and carried nothing more: its rate is
        // low, the highest is not, and the round trip is the least.
        link.round(560_000, ms(220));
        assert_eq!(link.window(), steady);
    }

    #[test]
    fn the_connection_drains_when_the_round_trip_has_stood_its_time() {
        let mut link = Link::new();
        for _ in 0..12 {
            link.round(560_000, ms(70));
        }
        // Rounds that are all longer now, a queue of the window's own: the
        // round trip stays until it has stood its time.
        let start = link.now;
        while !link.rtt.draining() {
            link.round(800_000, ms(100));
            assert!(link.now.duration_since(start) <= RTT_KEPT + ms(100));
        }
        assert!(link.now.duration_since(start) > RTT_KEPT);
        // Then the windows are the least, until a credit given in the drain
        // comes back: the round trip it took is the round trip.
        assert!(link.rtt.draining());
        assert_eq!(link.window(), LEAST);
        link.round(u64::from(LEAST), ms(90));
        assert!(!link.rtt.draining());
        assert_eq!(link.rtt.get(), Some(ms(90)));
        assert!(link.window() > LEAST);
    }

    #[test]
    fn a_sample_close_to_the_round_trip_keeps_the_drain_away() {
        let mut rtt = Rtt::new();
        let t = Instant::now();
        rtt.sample(ms(70), t, false);
        // A sixteenth over it says the round trip still is the round trip.
        rtt.sample(ms(74), t + ms(9_000), false);
        rtt.look(t + ms(15_000));
        assert!(!rtt.draining());
        assert_eq!(rtt.get(), Some(ms(70)));
        // More than that does not.
        rtt.sample(ms(75), t + ms(18_000), false);
        rtt.look(t + ms(19_001));
        assert!(rtt.draining());
    }

    #[test]
    fn a_path_that_became_slower_does_not_shrink_the_window_and_is_learned() {
        let mut link = Link::new();
        for _ in 0..12 {
            link.round(560_000, ms(70));
        }
        let before = link.window();
        // The path takes 350 ms now, whatever the window: each round carries
        // the window, and the rate it goes at is a fifth of the link's.
        let start = link.now;
        while !link.rtt.draining() {
            link.round(u64::from(link.window()), ms(350));
            let measured = link.tuner.measured();
            assert!(measured >= before, "{measured} under {before}");
            assert!(link.now.duration_since(start) <= RTT_KEPT + ms(350));
        }
        // The drain learns the slower round trip, and the window grows to it.
        let held = link.tuner.measured();
        link.round(u64::from(LEAST), ms(350));
        assert_eq!(link.rtt.get(), Some(ms(350)));
        link.round(u64::from(held), ms(350));
        assert!(link.window() > held, "{} not over {held}", link.window());
    }

    #[test]
    fn a_credit_from_before_the_drain_does_not_end_it() {
        let mut rtt = Rtt::new();
        let t = Instant::now();
        rtt.sample(ms(70), t, false);
        rtt.look(t + RTT_KEPT + ms(1));
        assert!(rtt.draining());
        // Larger than the least and not from the drain: the queue is still in
        // it.
        rtt.sample(ms(200), t + RTT_KEPT + ms(100), false);
        assert!(rtt.draining());
        assert_eq!(rtt.get(), Some(ms(70)));
        // A smaller one ends it whenever it comes.
        rtt.sample(ms(60), t + RTT_KEPT + ms(200), false);
        assert!(!rtt.draining());
        assert_eq!(rtt.get(), Some(ms(60)));
    }

    #[test]
    fn only_a_byte_past_the_timed_credit_is_a_sample() {
        let mut rtt = Rtt::new();
        let mut tuner = Tuner::new(START);
        let t = Instant::now();
        tuner.crediting(1000, t, &rtt);
        // A second credit while the first is timed is not timed.
        tuner.crediting(5000, t + ms(1), &rtt);
        tuner.received(1000, t + ms(30), &mut rtt, CAP);
        assert_eq!(rtt.get(), None, "the limit itself is not past it");
        tuner.received(1001, t + ms(65), &mut rtt, CAP);
        assert_eq!(rtt.get(), Some(ms(65)));
        // With nothing timed, bytes are not samples.
        tuner.received(9000, t + ms(500), &mut rtt, CAP);
        assert_eq!(rtt.get(), Some(ms(65)));
    }

    #[test]
    fn the_window_is_within_the_least_and_the_cap() {
        let mut rtt = Rtt::new();
        let mut tuner = Tuner::new(START);
        let t = Instant::now();
        // A trickle: 1 KB a round.
        for i in 0..5u64 {
            tuner.crediting(i * 1000, t + ms(100 * i), &rtt);
            tuner.received(i * 1000 + 1000, t + ms(100 * i + 70), &mut rtt, CAP);
        }
        assert_eq!(tuner.window(&rtt, CAP), LEAST);
        // A cap under the least is the window.
        assert_eq!(tuner.window(&rtt, 1000), 1000);
    }
}
