//! The conversations of mail: what the bridge keeps of each stream a program
//! on the server has opened on its mailbox, from the ticket to the end.
//!
//! A conversation joins two streams, one on a connection to the mailbox (the
//! sender's) and one on the connection to the bridge's client (the app's). It
//! begins when the sender opens its stream, is *located* once the terminal the
//! sender runs in is known and the app has been given a ticket for it, and is
//! *joined* when the app answers the ticket by opening its own stream. From
//! then on a `data`, `credit`, `eof` or `close` from one end is the same
//! message to the other, with the other's stream number; the bytes are not
//! looked at and none are kept here.
//!
//! The windows are the two ends' own: the one each gave when it opened is what
//! the other may send, and each `credit` goes through as it came. What the
//! table does is hold the ends to them, since it is the only place that sees
//! both: an end that sends more than it was given, a `data` over
//! [`wire::MAX_DATA`], a `credit` that takes the other's window over
//! [`wire::MAX_WINDOW`], or a `data` after its own `eof` loses the conversation
//! (both ends are sent `close`). That is also what keeps the bytes waiting in
//! the bridge for a conversation within the window the end that is slow to take
//! them gave.
//!
//! There is no I/O here. A call says what is to be sent to whom in a [`Said`],
//! and the mailbox, which owns the sockets, sends it.

use kablo_proto::SessionId;
use kablo_proto::v1::stream::{self as wire, FailCode, MAX_DATA, MAX_STREAMS, MAX_WINDOW};
use kablo_proto::v1::{Message, Packet};

/// The longest text of a `decline` that is passed on to a sender, in bytes.
const MAX_TEXT: usize = 4096;

/// What is to be said after a call.
#[derive(Debug, Default)]
pub struct Said {
    /// To a connection to the mailbox, by its number: a packet of the stream
    /// channel.
    pub to_senders: Vec<(u64, wire::Packet)>,
    /// To the app: a packet of the stream channel.
    pub to_app: Vec<wire::Packet>,
    /// To the app: the tickets, on the session channel.
    pub tickets: Vec<Packet>,
}

impl Said {
    fn tell_sender(&mut self, sender: u64, stream: u32, message: wire::Message) {
        self.to_senders
            .push((sender, wire::Packet::new(stream, message)));
    }

    fn tell_app(&mut self, stream: u32, message: wire::Message) {
        self.to_app.push(wire::Packet::new(stream, message));
    }

    /// Fails the sender's `open` of this stream.
    pub fn failed(&mut self, sender: u64, stream: u32, code: FailCode, text: &str) {
        self.tell_sender(
            sender,
            stream,
            wire::Message::Failed {
                code,
                text: text.to_owned(),
            },
        );
    }
}

/// One direction of a joined conversation.
#[derive(Clone, Copy, Debug)]
struct Leg {
    /// The bytes this end may still send before it has to wait for a credit.
    left: u32,
    /// Whether it has said it sends no more.
    eof: bool,
}

#[derive(Debug)]
enum State {
    /// The sender has opened its stream; the terminal it runs in is being
    /// found.
    Locating,
    /// The app has been given this ticket and has not answered it.
    Waiting { ticket: u32 },
    /// The app opened its stream `app`.
    Joined {
        app: u32,
        from_sender: Leg,
        from_app: Leg,
    },
}

#[derive(Debug)]
struct Conversation {
    /// The connection to the mailbox, by its number.
    sender: u64,
    /// The sender's stream.
    stream: u32,
    /// What the sender gave when it opened: the bytes the app may send first.
    window: u32,
    state: State,
}

/// Which end a message is from.
#[derive(Clone, Copy)]
enum End {
    Sender,
    App,
}

/// Every conversation of a bridge.
#[derive(Debug, Default)]
pub struct Conversations {
    list: Vec<Conversation>,
    /// The last ticket given. Tickets count up from 1 and none is used twice.
    last_ticket: u32,
}

impl Conversations {
    /// None at all.
    pub fn new() -> Conversations {
        Conversations::default()
    }

    /// How many conversations there are, in whatever state.
    pub fn len(&self) -> usize {
        self.list.len()
    }

    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    /// How many have the app's stream: the ones that count against the limit
    /// on the streams of the connection to the app.
    pub fn joined(&self) -> usize {
        self.list
            .iter()
            .filter(|c| matches!(c.state, State::Joined { .. }))
            .count()
    }

    /// Whether `stream` is the app's stream of a conversation.
    pub fn owns(&self, stream: u32) -> bool {
        self.joined_by_app(stream).is_some()
    }

    fn find(&self, sender: u64, stream: u32) -> Option<usize> {
        self.list
            .iter()
            .position(|c| c.sender == sender && c.stream == stream)
    }

    fn joined_by_app(&self, stream: u32) -> Option<usize> {
        self.list
            .iter()
            .position(|c| matches!(c.state, State::Joined { app, .. } if app == stream))
    }

    /// The sender `sender` opened its stream `stream` and gave `window`. An error
    /// is what to answer it with: the connection is full, or the window is more
    /// than any end takes.
    pub fn begin(
        &mut self,
        sender: u64,
        stream: u32,
        window: u32,
    ) -> Result<(), (FailCode, &'static str)> {
        if window > MAX_WINDOW {
            return Err((FailCode::LIMIT, "window too large"));
        }
        let of_sender = self.list.iter().filter(|c| c.sender == sender).count();
        if of_sender >= MAX_STREAMS as usize || self.list.len() >= MAX_STREAMS as usize {
            return Err((FailCode::LIMIT, "too many streams"));
        }
        self.list.push(Conversation {
            sender,
            stream,
            window,
            state: State::Locating,
        });
        Ok(())
    }

    /// The terminal the sender runs in is `terminal` (none: it runs in none):
    /// the app is given a ticket, with the terminal as its session.
    pub fn located(
        &mut self,
        sender: u64,
        stream: u32,
        terminal: Option<SessionId>,
        said: &mut Said,
    ) {
        let Some(i) = self.find(sender, stream) else {
            return;
        };
        let Some(ticket) = self.last_ticket.checked_add(1) else {
            return self.refuse(sender, stream, FailCode::LIMIT, "no tickets left", said);
        };
        let Some(conversation) = self.list.get_mut(i) else {
            return;
        };
        if !matches!(conversation.state, State::Locating) {
            return;
        }
        self.last_ticket = ticket;
        conversation.state = State::Waiting { ticket };
        said.tickets.push(Packet {
            session: terminal,
            message: Message::Ticket(ticket),
        });
    }

    /// Ends a conversation that has no ticket yet, answering the sender's
    /// `open` with a failure.
    pub fn refuse(
        &mut self,
        sender: u64,
        stream: u32,
        code: FailCode,
        text: &str,
        said: &mut Said,
    ) {
        if let Some(i) = self.find(sender, stream) {
            self.list.remove(i);
            said.failed(sender, stream, code, text);
        }
    }

    /// The app opened its stream `stream` to answer `ticket`, with `window`;
    /// `held` is how many other streams the connection to the app has. The
    /// ticket is answered whatever comes of it, and a ticket that is not
    /// outstanding is a failed `open`.
    pub fn answer(&mut self, stream: u32, window: u32, ticket: u32, held: usize, said: &mut Said) {
        let waiting = self
            .list
            .iter()
            .position(|c| matches!(c.state, State::Waiting { ticket: t } if t == ticket));
        let Some(i) = waiting else {
            return said.tell_app(
                stream,
                wire::Message::Failed {
                    code: FailCode::BAD_REQUEST,
                    text: "no such ticket".to_owned(),
                },
            );
        };
        let too_many = held.saturating_add(self.joined()) >= MAX_STREAMS as usize;
        let Some(conversation) = self.list.get_mut(i) else {
            return;
        };
        let (sender, sender_stream) = (conversation.sender, conversation.stream);
        let why = if window > MAX_WINDOW {
            Some((FailCode::LIMIT, "window too large"))
        } else if too_many {
            Some((FailCode::LIMIT, "too many streams"))
        } else {
            None
        };
        if let Some((code, text)) = why {
            self.list.remove(i);
            said.tell_app(
                stream,
                wire::Message::Failed {
                    code,
                    text: text.to_owned(),
                },
            );
            return said.failed(
                sender,
                sender_stream,
                code,
                &format!("the app could not take it: {text}"),
            );
        }
        let given = conversation.window;
        conversation.state = State::Joined {
            app: stream,
            from_sender: Leg {
                left: window,
                eof: false,
            },
            from_app: Leg {
                left: given,
                eof: false,
            },
        };
        said.tell_app(stream, wire::Message::Opened { window: given });
        said.tell_sender(sender, sender_stream, wire::Message::Opened { window });
    }

    /// The app refused `ticket`, in its own words.
    pub fn decline(&mut self, ticket: u32, text: String, said: &mut Said) {
        let waiting = self
            .list
            .iter()
            .position(|c| matches!(c.state, State::Waiting { ticket: t } if t == ticket));
        if let Some(c) = waiting.map(|i| self.list.remove(i)) {
            said.failed(c.sender, c.stream, FailCode::DECLINED, clip(&text));
        }
    }

    /// A stream message from the sender about its stream. One that is not
    /// `close` is not valid before the conversation is joined.
    pub fn from_sender(
        &mut self,
        sender: u64,
        stream: u32,
        message: wire::Message,
        said: &mut Said,
    ) {
        let Some(i) = self.find(sender, stream) else {
            return;
        };
        let joined = self
            .list
            .get(i)
            .is_some_and(|c| matches!(c.state, State::Joined { .. }));
        if joined {
            return self.pass(i, End::Sender, message, said);
        }
        // The ticket, if there is one, is forgotten.
        self.list.remove(i);
        if !matches!(message, wire::Message::Close) {
            said.tell_sender(sender, stream, wire::Message::Close);
        }
    }

    /// A stream message from the app about its stream `stream`, which is one
    /// of a conversation.
    pub fn from_app(&mut self, stream: u32, message: wire::Message, said: &mut Said) {
        if let Some(i) = self.joined_by_app(stream) {
            self.pass(i, End::App, message, said);
        }
    }

    /// The connections to the mailbox that `alive` does not name have gone: their
    /// tickets are forgotten and the app is told of the streams that were open,
    /// with a `close`.
    pub fn gone(&mut self, alive: impl Fn(u64) -> bool, said: &mut Said) {
        self.list.retain(|c| {
            if alive(c.sender) {
                return true;
            }
            if let State::Joined { app, .. } = c.state {
                said.tell_app(app, wire::Message::Close);
            }
            false
        });
    }

    /// Takes the conversation at `i` away, and tells both ends that it is over
    /// by `close`.
    fn lose(&mut self, i: usize, said: &mut Said) {
        let c = self.list.remove(i);
        if let State::Joined { app, .. } = c.state {
            said.tell_app(app, wire::Message::Close);
        }
        said.tell_sender(c.sender, c.stream, wire::Message::Close);
    }

    /// Passes a message of a joined conversation on to the other end, if it is
    /// a valid one from this.
    fn pass(&mut self, i: usize, from: End, message: wire::Message, said: &mut Said) {
        let Some(c) = self.list.get_mut(i) else {
            return;
        };
        let (sender, stream) = (c.sender, c.stream);
        let State::Joined {
            app,
            from_sender,
            from_app,
        } = &mut c.state
        else {
            return;
        };
        let app = *app;
        let (mine, theirs) = match from {
            End::Sender => (from_sender, from_app),
            End::App => (from_app, from_sender),
        };
        let valid = match &message {
            wire::Message::Data(bytes) => {
                let size = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
                if mine.eof || bytes.len() > MAX_DATA || size > mine.left {
                    false
                } else {
                    mine.left -= size;
                    true
                }
            }
            // The bytes an end has taken are a window for the other to send.
            wire::Message::Credit(bytes) => match theirs.left.checked_add(*bytes) {
                Some(left) if left <= MAX_WINDOW => {
                    theirs.left = left;
                    true
                }
                _ => false,
            },
            wire::Message::Eof => !std::mem::replace(&mut mine.eof, true),
            wire::Message::Close => true,
            // What an end sends that means nothing to a conversation.
            _ => return,
        };
        if !valid {
            return self.lose(i, said);
        }
        let over = match message {
            wire::Message::Close => true,
            wire::Message::Eof => theirs.eof,
            _ => false,
        };
        match from {
            End::Sender => said.tell_app(app, message),
            End::App => said.tell_sender(sender, stream, message),
        }
        if over {
            self.list.remove(i);
        }
    }
}

/// The text of a `decline` as far as a sender is told it.
fn clip(text: &str) -> &str {
    let mut end = text.len().min(MAX_TEXT);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.get(..end).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SENDER: u64 = 7;

    fn data(n: usize) -> wire::Message {
        wire::Message::Data(vec![0; n])
    }

    /// A conversation joined: the sender's stream 1 and the app's stream 5, the
    /// sender giving `to_sender` and the app `to_app`.
    fn joined(to_sender: u32, to_app: u32) -> Conversations {
        let mut table = Conversations::new();
        let mut said = Said::default();
        table.begin(SENDER, 1, to_sender).unwrap();
        table.located(SENDER, 1, None, &mut said);
        table.answer(5, to_app, 1, 0, &mut said);
        assert_eq!(table.joined(), 1);
        table
    }

    #[test]
    fn a_ticket_is_given_once_the_terminal_is_known_and_counts_up() {
        let mut table = Conversations::new();
        let mut said = Said::default();
        table.begin(SENDER, 1, 100).unwrap();
        table.begin(SENDER, 2, 100).unwrap();
        assert!(said.tickets.is_empty());
        let terminal = SessionId::new("work").unwrap();
        table.located(SENDER, 1, Some(terminal.clone()), &mut said);
        table.located(SENDER, 2, None, &mut said);
        // Located twice is no second ticket.
        table.located(SENDER, 2, None, &mut said);
        assert_eq!(
            said.tickets,
            [
                Packet {
                    session: Some(terminal),
                    message: Message::Ticket(1)
                },
                Packet {
                    session: None,
                    message: Message::Ticket(2)
                },
            ]
        );
    }

    #[test]
    fn answering_a_ticket_opens_both_ends_with_the_windows_the_other_gave() {
        let mut table = Conversations::new();
        let mut said = Said::default();
        table.begin(SENDER, 1, 1000).unwrap();
        table.located(SENDER, 1, None, &mut said);
        table.answer(5, 2000, 1, 0, &mut said);
        assert_eq!(
            said.to_app,
            [wire::Packet::new(5, wire::Message::Opened { window: 1000 })]
        );
        assert_eq!(
            said.to_senders,
            [(
                SENDER,
                wire::Packet::new(1, wire::Message::Opened { window: 2000 })
            )]
        );
        assert!(table.owns(5) && !table.owns(1));
    }

    #[test]
    fn a_ticket_that_is_not_outstanding_is_a_failed_open_and_is_answered_once() {
        let mut table = Conversations::new();
        let mut said = Said::default();
        table.answer(3, 10, 9, 0, &mut said);
        table.begin(SENDER, 1, 10).unwrap();
        table.located(SENDER, 1, None, &mut said);
        said.to_app.clear();
        table.answer(4, 10, 1, 0, &mut said);
        table.answer(5, 10, 1, 0, &mut said);
        let failed: Vec<u32> = said
            .to_app
            .iter()
            .filter(|p| matches!(p.message, wire::Message::Failed { code, .. } if code == FailCode::BAD_REQUEST))
            .map(|p| p.stream)
            .collect();
        assert_eq!(failed, [5]);
    }

    #[test]
    fn a_decline_fails_the_senders_open_with_the_apps_words() {
        let mut table = Conversations::new();
        let mut said = Said::default();
        table.begin(SENDER, 1, 10).unwrap();
        table.located(SENDER, 1, None, &mut said);
        table.decline(1, "not now".to_owned(), &mut said);
        assert_eq!(
            said.to_senders,
            [(
                SENDER,
                wire::Packet::new(
                    1,
                    wire::Message::Failed {
                        code: FailCode::DECLINED,
                        text: "not now".to_owned()
                    }
                )
            )]
        );
        assert!(table.is_empty());
    }

    #[test]
    fn the_text_of_a_decline_is_clipped_on_a_character() {
        let long = "é".repeat(MAX_TEXT);
        let clipped = clip(&long);
        assert!(clipped.len() <= MAX_TEXT && !clipped.is_empty());
        assert!(long.starts_with(clipped));
    }

    #[test]
    fn bytes_go_both_ways_under_the_window_the_other_gave() {
        // The sender gave 100 (the app may send it that many), the app 50.
        let mut table = joined(100, 50);
        let mut said = Said::default();
        table.from_sender(SENDER, 1, data(50), &mut said);
        table.from_app(5, data(100), &mut said);
        assert_eq!(said.to_app, [wire::Packet::new(5, data(50))]);
        assert_eq!(said.to_senders, [(SENDER, wire::Packet::new(1, data(100)))]);
        // Out of window: a credit makes room.
        said = Said::default();
        table.from_app(5, wire::Message::Credit(10), &mut said);
        table.from_sender(SENDER, 1, data(10), &mut said);
        assert_eq!(
            (said.to_app.len(), said.to_senders.len()),
            (1, 1),
            "{said:?}"
        );
        assert_eq!(table.joined(), 1);
    }

    #[test]
    fn an_end_that_sends_more_than_it_was_given_loses_the_conversation() {
        let mut table = joined(100, 50);
        let mut said = Said::default();
        table.from_sender(SENDER, 1, data(51), &mut said);
        assert_eq!(said.to_app, [wire::Packet::new(5, wire::Message::Close)]);
        assert_eq!(
            said.to_senders,
            [(SENDER, wire::Packet::new(1, wire::Message::Close))]
        );
        assert!(table.is_empty());
    }

    #[test]
    fn what_breaks_the_rules_of_a_stream_loses_the_conversation() {
        let breaks: [(End, wire::Message); 4] = [
            (End::Sender, data(MAX_DATA + 1)),
            (End::App, wire::Message::Credit(u32::MAX)),
            (End::App, wire::Message::Credit(MAX_WINDOW)),
            (End::Sender, wire::Message::Eof),
        ];
        for (from, message) in breaks {
            let mut table = joined(MAX_WINDOW, MAX_WINDOW);
            let mut said = Said::default();
            if matches!(message, wire::Message::Eof) {
                // A second `eof`, and a `data` after the first.
                table.from_sender(SENDER, 1, wire::Message::Eof, &mut said);
                said = Said::default();
                table.from_sender(SENDER, 1, data(1), &mut said);
            } else {
                match from {
                    End::Sender => table.from_sender(SENDER, 1, message, &mut said),
                    End::App => table.from_app(5, message, &mut said),
                }
            }
            assert!(table.is_empty(), "{said:?}");
            assert_eq!(said.to_app.len(), 1);
            assert_eq!(said.to_senders.len(), 1);
        }
    }

    #[test]
    fn a_conversation_ends_with_a_close_or_with_both_eofs() {
        let mut table = joined(100, 100);
        let mut said = Said::default();
        table.from_sender(SENDER, 1, wire::Message::Eof, &mut said);
        assert_eq!(table.joined(), 1);
        table.from_app(5, data(3), &mut said);
        table.from_app(5, wire::Message::Eof, &mut said);
        assert!(table.is_empty(), "both ends said they send no more");
        // Frames of a conversation that is over are ignored.
        said = Said::default();
        table.from_app(5, wire::Message::Close, &mut said);
        table.from_sender(SENDER, 1, wire::Message::Close, &mut said);
        assert!(said.to_app.is_empty() && said.to_senders.is_empty());

        let mut table = joined(100, 100);
        table.from_app(5, wire::Message::Close, &mut said);
        assert_eq!(
            said.to_senders,
            [(SENDER, wire::Packet::new(1, wire::Message::Close))]
        );
        assert!(table.is_empty());
    }

    #[test]
    fn a_sender_that_gives_up_has_its_ticket_forgotten_and_its_stream_closed_for_the_app() {
        let mut table = Conversations::new();
        let mut said = Said::default();
        table.begin(SENDER, 1, 10).unwrap();
        table.located(SENDER, 1, None, &mut said);
        table.from_sender(SENDER, 1, wire::Message::Close, &mut said);
        assert!(table.is_empty() && said.to_senders.is_empty());
        // The app's late answer is a failed open.
        table.answer(5, 10, 1, 0, &mut said);
        assert!(matches!(
            said.to_app.last().map(|p| &p.message),
            Some(wire::Message::Failed { .. })
        ));

        let mut table = joined(10, 10);
        let mut said = Said::default();
        table.gone(|_| false, &mut said);
        assert_eq!(said.to_app, [wire::Packet::new(5, wire::Message::Close)]);
        assert!(table.is_empty());
    }

    #[test]
    fn data_before_the_conversation_is_joined_is_not_valid() {
        let mut table = Conversations::new();
        let mut said = Said::default();
        table.begin(SENDER, 1, 10).unwrap();
        table.from_sender(SENDER, 1, data(1), &mut said);
        assert!(table.is_empty());
        assert_eq!(
            said.to_senders,
            [(SENDER, wire::Packet::new(1, wire::Message::Close))]
        );
    }

    #[test]
    fn the_streams_of_a_connection_are_limited_and_so_are_the_windows() {
        let mut table = Conversations::new();
        assert_eq!(
            table.begin(SENDER, 1, MAX_WINDOW + 1),
            Err((FailCode::LIMIT, "window too large"))
        );
        for stream in 1..=MAX_STREAMS {
            table.begin(SENDER, stream, 10).unwrap();
        }
        assert_eq!(
            table.begin(SENDER, 1000, 10),
            Err((FailCode::LIMIT, "too many streams"))
        );
        assert_eq!(
            table.begin(SENDER + 1, 1, 10),
            Err((FailCode::LIMIT, "too many streams"))
        );

        // The app's side counts the streams it has besides.
        let mut table = Conversations::new();
        let mut said = Said::default();
        table.begin(SENDER, 1, 10).unwrap();
        table.located(SENDER, 1, None, &mut said);
        table.answer(5, 10, 1, MAX_STREAMS as usize, &mut said);
        assert!(table.is_empty());
        assert_eq!(said.to_app.len(), 1);
        assert_eq!(said.to_senders.len(), 1);
        // The sender is told the app's limit, not that no app is there: the
        // first would be a conversation to try again, the second a reason to
        // look for another app.
        assert!(
            matches!(
                &said.to_senders[0].1.message,
                wire::Message::Failed {
                    code: FailCode::LIMIT,
                    ..
                }
            ),
            "{:?}",
            said.to_senders[0]
        );
    }
}
