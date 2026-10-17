//! `kablo forward`: the client end of port forwarding, for people and for
//! scripts that have no terminal app to do it.
//!
//! It runs a command that ends in a bridge (`ssh host kablo attach`), speaks
//! the protocol on that command's input and output, listens on local ports, and
//! carries every connection made to one of them to a `host:port` that the
//! server can reach, as `ssh -L` does: the server's sshd is asked for nothing,
//! so it works where sshd's own forwarding is off. The command is taken as it
//! is, so ssh's options, jump hosts, keys and passwords stay in the user's ssh
//! configuration and `ssh` prompts for what it needs on the terminal.
//!
//! The listening ports are taken **before** the command starts, so a port in
//! use is said at once and costs no login, and they are announced only once the
//! server has said it takes connections: a script waiting for the line
//! `forwarding ADDRESS to HOST:PORT` can connect as soon as it reads it. Local
//! port 0 lets the system choose; the line says which.
//!
//! By default only the loopback addresses listen (`127.0.0.1` and `::1`): a
//! forwarded port is a way into the server's network, and whoever can connect
//! to it uses that way. `--bind` names another address, on purpose.
//!
//! The windows, queues and sockets are [`crate::streams`] and
//! [`crate::outbound`], the same code the bridge runs; this module is the
//! loop around them (what to wait for, what arrived, what to say) and the
//! command line. It never reconnects: the stream's life is the carrier's, and
//! when the command ends the connections end with it, and so does this.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{self, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};

use kablo_proto::v1::stream::{self as wire, Incoming, IncomingDecoder, MAX_HOST};
use kablo_proto::v1::{Features, Message};

use crate::carrier::{self, READ_CHUNK, clean, describe, signal_in};
pub use crate::carrier::{End, Wire};
use crate::outbound::Outbound;
use crate::streams::{self, Event, Streams, Verdict};
use crate::sys;

/// What the `ping` that follows the `features` carries.
const PING: &[u8] = b"kablo forward";

// --- the command line ----------------------------------------------------------

/// One forwarding: connections to local port `local` go to `host:port` as the
/// server sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Spec {
    /// The local port; 0 lets the system choose.
    pub local: u16,
    /// A name or an address, resolved by the server (`localhost` is the
    /// server's).
    pub host: String,
    /// The port on the host; never 0.
    pub port: u16,
}

impl Spec {
    /// Reads `PORT` (the same port here and there, on the server's
    /// `localhost`) or `LOCAL:HOST:PORT`. An IPv6 host goes in brackets.
    pub fn parse(text: &str) -> Result<Spec, String> {
        let Some((local, rest)) = text.split_once(':') else {
            let port = remote_port(text)?;
            return Ok(Spec {
                local: port,
                host: "localhost".to_owned(),
                port,
            });
        };
        let local = local_port(local)?;
        let (host, port) = match rest.strip_prefix('[') {
            Some(bracketed) => {
                let (host, after) = bracketed
                    .split_once(']')
                    .ok_or_else(|| format!("{text}: the ] is missing"))?;
                let port = after
                    .strip_prefix(':')
                    .ok_or_else(|| format!("{text}: the port goes after ]:"))?;
                (host, port)
            }
            None => rest
                .rsplit_once(':')
                .ok_or_else(|| format!("{text}: expected PORT or LOCAL:HOST:PORT"))?,
        };
        if host.is_empty() || host.len() > MAX_HOST || host.chars().any(char::is_control) {
            return Err(format!("{text}: that is not a host name or address"));
        }
        if host.contains(':') && !rest.starts_with('[') {
            return Err(format!("{text}: write an IPv6 address in brackets"));
        }
        Ok(Spec {
            local,
            host: host.to_owned(),
            port: remote_port(port)?,
        })
    }

    /// `host:port` as a person reads it.
    pub fn target(&self) -> String {
        target(&self.host, self.port)
    }
}

fn target(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

fn remote_port(text: &str) -> Result<u16, String> {
    match text.parse::<u16>() {
        Ok(port) if port != 0 => Ok(port),
        _ => Err(format!("{text}: that is not a port (1 to 65535)")),
    }
}

fn local_port(text: &str) -> Result<u16, String> {
    text.parse::<u16>()
        .map_err(|_| format!("{text}: that is not a port (0 to 65535, 0 for any)"))
}

/// Where the local ports listen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bind {
    /// `127.0.0.1` and, where there is one, `::1`.
    Loopback,
    /// This address and no other.
    Address(IpAddr),
}

impl Bind {
    fn parse(text: &str) -> Result<Bind, String> {
        if text == "localhost" {
            return Ok(Bind::Loopback);
        }
        let bare = text
            .strip_prefix('[')
            .and_then(|inner| inner.strip_suffix(']'))
            .unwrap_or(text);
        bare.parse::<IpAddr>()
            .map(Bind::Address)
            .map_err(|_| format!("{text}: --bind takes an IP address or localhost"))
    }
}

/// What `kablo forward` was asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    /// Where the local ports listen.
    pub bind: Bind,
    /// What to forward.
    pub specs: Vec<Spec>,
    /// The command that reaches the bridge, program first.
    pub carrier: Vec<OsString>,
}

/// Reads the arguments after `kablo forward`:
/// `[--bind ADDRESS] SPEC... -- COMMAND [ARGUMENT]...`.
pub fn parse(args: &[OsString]) -> Result<Options, String> {
    let mut bind = Bind::Loopback;
    let mut specs = Vec::new();
    let mut words = args.iter();
    loop {
        let Some(word) = words.next() else {
            return Err("the command that reaches the server goes after `--`".to_owned());
        };
        let Some(text) = word.to_str() else {
            return Err("a forwarding is text".to_owned());
        };
        match text {
            "--" => break,
            "--bind" => {
                let value = words
                    .next()
                    .and_then(|value| value.to_str())
                    .ok_or_else(|| "--bind needs an address".to_owned())?;
                bind = Bind::parse(value)?;
            }
            flag if flag.starts_with('-') => return Err(format!("unknown option {flag}")),
            spec => specs.push(Spec::parse(spec)?),
        }
    }
    let carrier: Vec<OsString> = words.cloned().collect();
    if specs.is_empty() {
        return Err("nothing to forward: name a PORT or LOCAL:HOST:PORT".to_owned());
    }
    if carrier.is_empty() {
        return Err("the command after `--` is missing".to_owned());
    }
    Ok(Options {
        bind,
        specs,
        carrier,
    })
}

// --- listening -----------------------------------------------------------------

/// A local port that is listening, and where its connections go.
#[derive(Debug)]
pub struct Listening {
    socket: TcpListener,
    address: SocketAddr,
    host: String,
    port: u16,
}

impl Listening {
    /// The address that listens (the port is the one the system chose, for a
    /// local port of 0).
    pub fn address(&self) -> SocketAddr {
        self.address
    }
}

fn bind_one(ip: IpAddr, port: u16) -> io::Result<TcpListener> {
    let socket = TcpListener::bind((ip, port))?;
    socket.set_nonblocking(true)?;
    Ok(socket)
}

fn cannot_listen(address: SocketAddr, error: &io::Error) -> String {
    let why = match error.kind() {
        io::ErrorKind::AddrInUse => "the address is already in use".to_owned(),
        io::ErrorKind::PermissionDenied => "permission denied".to_owned(),
        _ => error.to_string(),
    };
    format!("cannot listen on {address}: {why}")
}

/// Starts listening for every spec. Nothing listens if one cannot: a
/// forwarding that is only partly there is a surprise (`ssh`'s
/// `ExitOnForwardFailure`).
///
/// On the loopback the IPv4 address must listen and the IPv6 one does where it
/// can: a machine without IPv6 is not an error, but a fixed port that something
/// else holds on `::1` is, because `localhost` would reach that something.
pub fn listen(bind: Bind, specs: &[Spec]) -> Result<Vec<Listening>, String> {
    let mut found = Vec::new();
    for spec in specs {
        let first = match bind {
            Bind::Loopback => IpAddr::V4(Ipv4Addr::LOCALHOST),
            Bind::Address(ip) => ip,
        };
        let socket = bind_one(first, spec.local)
            .map_err(|e| cannot_listen(SocketAddr::new(first, spec.local), &e))?;
        let address = socket
            .local_addr()
            .map_err(|e| cannot_listen(SocketAddr::new(first, spec.local), &e))?;
        found.push(Listening {
            socket,
            address,
            host: spec.host.clone(),
            port: spec.port,
        });
        if bind != Bind::Loopback {
            continue;
        }
        let v6 = IpAddr::V6(Ipv6Addr::LOCALHOST);
        match bind_one(v6, address.port()) {
            Ok(socket) => found.push(Listening {
                socket,
                address: SocketAddr::new(v6, address.port()),
                host: spec.host.clone(),
                port: spec.port,
            }),
            Err(e) if e.kind() == io::ErrorKind::AddrInUse && spec.local != 0 => {
                return Err(cannot_listen(SocketAddr::new(v6, address.port()), &e));
            }
            // No IPv6 here, or a port that was picked for us and is taken on
            // `::1`: the IPv4 address serves.
            Err(_) => {}
        }
    }
    Ok(found)
}

// --- the carrier -----------------------------------------------------------------

/// What a descriptor in the poll set stands for.
enum Source {
    Signal,
    Carrier,
    Out,
    Listener(usize),
    Stream(streams::Token),
}

fn verdict(first: &Message) -> Result<Features, End> {
    match streams::judge(first) {
        Verdict::Streams(features) => Ok(features),
        Verdict::Without(Some(reason)) => Err(End::Declined(format!(
            "the server does not carry connections: {}",
            clean(&reason)
        ))),
        Verdict::Without(None) => Err(End::Declined(
            "the kablo on the server is too old to carry connections: it needs a newer one"
                .to_owned(),
        )),
    }
}

/// Carries the connections made to `listeners` over `carrier` until something
/// ends it. `out` is where the announcement of what listens goes (one line per
/// address, once the server has said it takes connections); `err` is where a
/// connection that could not be carried is said.
///
/// `signals`, when there is one, is the read end of a pipe of signal numbers
/// (see [`sys::signal_pipe`]); the first one read ends this.
pub fn serve(
    carrier: Wire,
    listeners: Vec<Listening>,
    signals: Option<BorrowedFd<'_>>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> End {
    let mut decoder = IncomingDecoder::new();
    let features = match carrier::negotiate(&carrier, signals, &mut decoder, PING, verdict) {
        Ok(features) => features,
        Err(end) => return end,
    };
    for listening in &listeners {
        let _ = writeln!(
            out,
            "forwarding {} to {}",
            listening.address,
            target(&listening.host, listening.port)
        );
    }
    let _ = out.flush();

    // The limits are the server's, within this build's. (A smaller `max_data`
    // than the protocol's is not honoured: no bridge says one.)
    let mut config = streams::Config::default();
    if features.max_window != 0 {
        config.window = config.window.min(features.max_window);
    }
    if let Ok(max) = usize::try_from(features.max_streams)
        && max != 0
    {
        config.max_streams = config.max_streams.min(max);
    }
    let Wire { input, output } = carrier;
    for fd in [&input, &output] {
        if let Err(e) = sys::set_nonblocking(fd.as_raw_fd()) {
            return End::Failed(format!("cannot set up the command's pipes: {e}"));
        }
    }
    Forwarding {
        input,
        output,
        listeners,
        signals,
        decoder,
        out: Outbound::new(READ_CHUNK),
        max_streams: config.max_streams,
        max_connecting: config.max_connecting,
        streams: Streams::new(wire::Side::Client, config),
        control: Vec::new(),
        labels: BTreeMap::new(),
        buf: vec![0u8; READ_CHUNK],
        err,
    }
    .run()
}

struct Forwarding<'a> {
    input: OwnedFd,
    output: OwnedFd,
    listeners: Vec<Listening>,
    signals: Option<BorrowedFd<'a>>,
    decoder: IncomingDecoder,
    out: Outbound,
    streams: Streams,
    max_streams: usize,
    max_connecting: usize,
    /// What the streams said this turn that is not `data`.
    control: Vec<wire::Packet>,
    /// Where the streams that were opened and not answered are going.
    labels: BTreeMap<u32, String>,
    buf: Vec<u8>,
    err: &'a mut dyn Write,
}

impl Forwarding<'_> {
    fn run(&mut self) -> End {
        loop {
            let mut set = Vec::new();
            let mut sources = Vec::new();
            if let Some(signals) = self.signals {
                set.push(sys::want(signals.as_raw_fd(), libc::POLLIN));
                sources.push(Source::Signal);
            }
            set.push(sys::want(self.input.as_raw_fd(), libc::POLLIN));
            sources.push(Source::Carrier);
            if self.out.pending() > 0 {
                set.push(sys::want(self.output.as_raw_fd(), libc::POLLOUT));
                sources.push(Source::Out);
            }
            // A connection that is not accepted waits in the system's queue,
            // which is what a server that is busy should look like to it.
            if self.may_accept() {
                for (i, listening) in self.listeners.iter().enumerate() {
                    set.push(sys::want(listening.socket.as_raw_fd(), libc::POLLIN));
                    sources.push(Source::Listener(i));
                }
            }
            for (fd, token) in self.streams.interest() {
                set.push(fd);
                sources.push(Source::Stream(token));
            }
            if let Err(e) = sys::poll(&mut set, None) {
                return End::Failed(format!("cannot wait for connections: {e}"));
            }
            for (fd, source) in set.iter().zip(&sources) {
                if fd.revents == 0 {
                    continue;
                }
                match source {
                    Source::Signal => {
                        if let Some(signal) = self.signals.and_then(signal_in) {
                            return End::Signal(signal);
                        }
                    }
                    Source::Carrier => {
                        if let Some(end) = self.read_carrier() {
                            return end;
                        }
                    }
                    Source::Out => {}
                    Source::Listener(i) => {
                        if let Some(end) = self.accept(*i) {
                            return end;
                        }
                    }
                    Source::Stream(token) => {
                        self.streams.ready(*token, fd.revents, &mut self.control);
                    }
                }
            }
            self.streams.settle(&mut self.control);
            self.report();
            for packet in std::mem::take(&mut self.control) {
                let _ = packet.encode(wire::Side::Client, self.out.buffer());
            }
            // The bridge's rule, from the same code: a stream's frame goes in
            // behind the control frames every turn, unless the command is so far
            // behind that more would only wait.
            self.out.end_turn(&mut self.streams);
            let output = self.output.as_fd();
            if !self
                .out
                .flush(&mut self.streams, &mut |bytes| sys::write_fd(output, bytes))
            {
                return End::Carrier;
            }
        }
    }

    /// Whether another connection may be taken: the server connects only so
    /// many at once and has only so many open, and a connection past either is
    /// failed, where one that waits is served in its turn. (The server's own
    /// count can run a little ahead of this one, since it counts a stream
    /// until it has finished with it; a connection that meets the limit there
    /// is told so, and the others are not touched.)
    fn may_accept(&self) -> bool {
        self.streams.len() < self.max_streams && self.streams.opening() < self.max_connecting
    }

    fn read_carrier(&mut self) -> Option<End> {
        let n = match sys::read_fd(self.input.as_fd(), &mut self.buf) {
            Ok(0) => return Some(End::Carrier),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return None,
            Err(_) => return Some(End::Carrier),
        };
        self.decoder.push(self.buf.get(..n).unwrap_or_default());
        loop {
            match self.decoder.next_incoming() {
                Ok(Some(Incoming::Stream { from, packet })) => {
                    self.streams.handle(from, packet, &mut self.control);
                }
                Ok(Some(Incoming::Session(packet))) => {
                    // A `pong` or an `error` for something nobody here is
                    // waiting on.
                    if let Message::Error { text, .. } = packet.message {
                        let _ = writeln!(self.err, "kablo: the server said: {}", clean(&text));
                    }
                }
                Ok(None) => return None,
                Err(e) if e.is_recoverable() => {}
                // The framing is broken: there is no telling where the next
                // frame starts.
                Err(e) => {
                    return Some(End::Failed(format!(
                        "what the server sent is not the protocol: {e}"
                    )));
                }
            }
        }
    }

    fn accept(&mut self, index: usize) -> Option<End> {
        while self.may_accept() {
            let listening = self.listeners.get(index)?;
            match listening.socket.accept() {
                Ok((socket, _)) => {
                    let label = target(&listening.host, listening.port);
                    match self.streams.open(
                        socket,
                        &listening.host,
                        listening.port,
                        &mut self.control,
                    ) {
                        Ok(id) => {
                            self.labels.insert(id, label);
                        }
                        Err(e) => {
                            let _ = writeln!(self.err, "kablo: cannot forward to {label}: {e}");
                        }
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return None,
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::ConnectionAborted | io::ErrorKind::Interrupted
                    ) => {}
                // Anything else (out of descriptors, say) leaves the
                // connection in the queue and the listener readable: waiting
                // again would spin.
                Err(e) => {
                    return Some(End::Failed(format!(
                        "cannot accept connections on {}: {e}",
                        listening.address
                    )));
                }
            }
        }
        None
    }

    /// Says what the server said about the streams that were being opened.
    fn report(&mut self) {
        for event in self.streams.take_events() {
            match event {
                Event::Opened(id) => {
                    self.labels.remove(&id);
                }
                // Connections have none of these: they are a command's or a
                // conversation's.
                Event::Exited { .. } | Event::Over { .. } | Event::Closed { .. } => {}
                Event::Failed { stream, text, .. } => {
                    let label = self
                        .labels
                        .remove(&stream)
                        .unwrap_or_else(|| "the target".to_owned());
                    let _ = writeln!(
                        self.err,
                        "kablo: cannot forward to {label}: {}",
                        clean(&text)
                    );
                }
            }
        }
    }
}

// --- the command ----------------------------------------------------------------

/// Starts the carrier and forwards until something ends it. Returns the exit
/// status: 128 and the signal's number for a signal, 1 for everything else.
pub fn run(options: Options, out: &mut dyn Write, err: &mut dyn Write) -> u8 {
    let listeners = match listen(options.bind, &options.specs) {
        Ok(listeners) => listeners,
        Err(why) => {
            let _ = writeln!(err, "kablo: {why}");
            return 1;
        }
    };
    let signals = match sys::signal_pipe(&[libc::SIGINT, libc::SIGTERM, libc::SIGHUP]) {
        Ok(fd) => fd,
        Err(e) => {
            let _ = writeln!(err, "kablo: cannot watch for signals: {e}");
            return 1;
        }
    };
    let (mut command, wire) = match carrier::start(&options.carrier) {
        Ok(started) => started,
        Err(why) => {
            let _ = writeln!(err, "kablo: {why}");
            return 1;
        }
    };
    // The carrier's pipes are gone when this returns, and with them the
    // command's input: the end of it is how a bridge is told to finish.
    let end = serve(wire, listeners, Some(signals.as_fd()), out, err);
    let status = command.reap(end != End::Carrier);
    match end {
        End::Signal(signal) => u8::try_from(128 + signal).unwrap_or(1),
        End::Carrier => {
            let _ = writeln!(
                err,
                "kablo: {} {}; the connections it carried are closed",
                command.name(),
                describe(status)
            );
            1
        }
        End::Declined(why) | End::Failed(why) => {
            let _ = writeln!(err, "kablo: {why}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    fn spec(local: u16, host: &str, port: u16) -> Spec {
        Spec {
            local,
            host: host.to_owned(),
            port,
        }
    }

    #[test]
    fn a_port_alone_is_the_same_port_on_the_servers_localhost() {
        assert_eq!(Spec::parse("3000"), Ok(spec(3000, "localhost", 3000)));
    }

    #[test]
    fn a_forwarding_names_the_local_port_the_host_and_the_port() {
        assert_eq!(
            Spec::parse("8080:localhost:3000"),
            Ok(spec(8080, "localhost", 3000))
        );
        assert_eq!(
            Spec::parse("0:db.internal:5432"),
            Ok(spec(0, "db.internal", 5432)),
            "0 is for the system to choose"
        );
        assert_eq!(
            Spec::parse("9000:10.0.0.7:80"),
            Ok(spec(9000, "10.0.0.7", 80))
        );
    }

    #[test]
    fn an_ipv6_host_is_written_in_brackets_and_sent_without() {
        let parsed = Spec::parse("8080:[::1]:3000").unwrap();
        assert_eq!(parsed, spec(8080, "::1", 3000));
        assert_eq!(parsed.target(), "[::1]:3000");
        assert!(Spec::parse("8080:::1:3000").is_err());
        assert!(Spec::parse("8080:[::1:3000").is_err());
        assert!(Spec::parse("8080:[::1]3000").is_err());
    }

    #[test]
    fn what_is_not_a_forwarding_is_said() {
        for bad in [
            "",
            "0",
            "70000",
            "x",
            "8080:",
            "8080::80",
            ":host:80",
            "8080:host:0",
            "8080:host:x",
            "99999:host:80",
            "8080:ho\u{7}st:80",
        ] {
            assert!(Spec::parse(bad).is_err(), "{bad:?}");
        }
        let long = format!("1:{}:80", "a".repeat(MAX_HOST + 1));
        assert!(Spec::parse(&long).is_err(), "a host name longer than DNS's");
    }

    #[test]
    fn the_command_line_is_options_forwardings_and_after_two_dashes_the_command() {
        let options = parse(&words(&[
            "--bind",
            "127.0.0.1",
            "8080:localhost:3000",
            "9000",
            "--",
            "ssh",
            "-T",
            "devbox",
            "kablo",
            "attach",
        ]))
        .unwrap();
        assert_eq!(options.bind, Bind::Address(IpAddr::V4(Ipv4Addr::LOCALHOST)));
        assert_eq!(
            options.specs,
            [spec(8080, "localhost", 3000), spec(9000, "localhost", 9000)]
        );
        assert_eq!(
            options.carrier,
            words(&["ssh", "-T", "devbox", "kablo", "attach"])
        );

        let plain = parse(&words(&["80", "--", "kablo", "attach"])).unwrap();
        assert_eq!(plain.bind, Bind::Loopback, "loopback unless told otherwise");
    }

    #[test]
    fn a_command_line_that_asks_for_nothing_it_can_do_is_refused() {
        for bad in [
            &["--", "kablo", "attach"][..],
            &["80"],
            &["80", "--"],
            &["--bind", "80", "--", "x"],
            &["--bind"],
            &["--bind", "somewhere", "80", "--", "x"],
            &["--wat", "80", "--", "x"],
            &["nonsense", "--", "x"],
        ] {
            assert!(parse(&words(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn bind_takes_an_address_in_either_family_or_localhost() {
        assert_eq!(Bind::parse("localhost"), Ok(Bind::Loopback));
        assert_eq!(
            Bind::parse("0.0.0.0"),
            Ok(Bind::Address(IpAddr::V4(Ipv4Addr::UNSPECIFIED)))
        );
        assert_eq!(
            Bind::parse("::"),
            Ok(Bind::Address(IpAddr::V6(Ipv6Addr::UNSPECIFIED)))
        );
        assert_eq!(
            Bind::parse("[::1]"),
            Ok(Bind::Address(IpAddr::V6(Ipv6Addr::LOCALHOST)))
        );
        assert!(Bind::parse("example.com").is_err());
    }

    #[test]
    fn by_default_only_the_loopback_listens() {
        let found = listen(Bind::Loopback, &[spec(0, "localhost", 80)]).unwrap();
        assert!(!found.is_empty());
        for listening in &found {
            assert!(
                listening.address().ip().is_loopback(),
                "{}",
                listening.address()
            );
        }
        // IPv4 always; IPv6 where the machine has it, on the port IPv4 got.
        assert_eq!(found[0].address().ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert!(
            found
                .iter()
                .all(|l| l.address().port() == found[0].address().port())
        );
    }

    #[test]
    fn an_address_that_is_asked_for_is_the_only_one_listened_on() {
        // (The loopback, not the wildcard: a test that listens on every
        // address makes some systems ask whether to allow it.)
        let one = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let found = listen(Bind::Address(one), &[spec(0, "localhost", 80)]).unwrap();
        assert_eq!(found.len(), 1, "no second address is added");
        assert_eq!(found[0].address().ip(), one);
    }

    #[test]
    fn a_port_in_use_is_said_and_nothing_else_is_left_listening() {
        let taken = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = taken.local_addr().unwrap().port();
        let why = listen(
            Bind::Loopback,
            &[spec(0, "localhost", 1), spec(port, "localhost", 2)],
        )
        .unwrap_err();
        assert!(why.contains("already in use"), "{why}");
        assert!(why.contains(&port.to_string()), "{why}");
    }
}
