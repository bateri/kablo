//! `kablo list`: the sessions as a table for a person and as JSON for a
//! program.
//!
//! A session's program name and directory are bytes some other program chose,
//! and the table goes to a terminal: control characters are replaced before
//! anything is printed, or a directory named after an escape sequence would
//! write to the screen of whoever ran `kablo list`. So are the names the
//! attached clients gave themselves. The JSON is written by
//! hand (the keys are fixed and flat, but for the list of who is attached);
//! keys may be added to it, none removed.

use std::fmt::Write as _;

use kablo_proto::v1::{Exit, State, Status};

/// The state as a word: `running`, `ended`, `broken`, or `unknown(N)` for a
/// number this build has no name for.
pub fn state_name(state: State) -> String {
    match state {
        State::RUNNING => "running".to_owned(),
        State::ENDED => "ended".to_owned(),
        State::BROKEN => "broken".to_owned(),
        State(other) => format!("unknown({other})"),
    }
}

/// Text safe to print: lossy UTF-8 with every control character replaced by
/// `?`.
pub fn printable(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

fn how_it_ended(exit: Exit) -> String {
    match exit {
        Exit::Code(code) => format!("exit {code}"),
        Exit::Signal(signal) => format!("signal {signal}"),
    }
}

/// The word in the state column.
fn state_cell(status: &Status) -> String {
    match (status.state, status.exit) {
        (State::ENDED, Some(exit)) => format!("ended ({})", how_it_ended(exit)),
        (state, _) => state_name(state),
    }
}

/// How long ago `from` was, at `now` (both in seconds since the Unix epoch):
/// `45s`, `12m`, `2h 14m`, `3d 4h`. `-` when it is not known, or lies after
/// `now` (the clocks of two machines do not agree to the second).
fn ago(from: Option<u64>, now: u64) -> String {
    let Some(seconds) = from.and_then(|from| now.checked_sub(from)) else {
        return "-".to_owned();
    };
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m", seconds / 60),
        3600..86_400 => format!("{}h {}m", seconds / 3600, seconds % 3600 / 60),
        _ => format!("{}d {}h", seconds / 86_400, seconds % 86_400 / 3600),
    }
}

/// Whether the relay told who is attached: it names every client it counts. A
/// relay from before the names counts and names no one.
fn says_who(status: &Status) -> bool {
    u32::try_from(status.clients_named.len()).is_ok_and(|named| named == status.clients)
}

/// The CLIENTS cell: the names the clients gave themselves, joined by commas,
/// then `+N` for the N that gave none (`MacBook,iMac`, `MacBook+1`). The count
/// when no one is named: no one is attached, or only clients with no name are
/// (`kablo attach` is one), or the relay does not say.
fn clients_cell(status: &Status) -> String {
    let names: Vec<String> = status
        .clients_named
        .iter()
        .flatten()
        .map(|name| printable(name.as_str().as_bytes()))
        .collect();
    if !says_who(status) || names.is_empty() {
        return status.clients.to_string();
    }
    let mut cell = names.join(",");
    let unnamed = status.clients_named.len() - names.len();
    if unnamed > 0 {
        let _ = write!(cell, "+{unnamed}");
    }
    cell
}

/// The table: a header, then a row per session; empty when there is none.
/// `now` (seconds since the Unix epoch, on the clock the sessions' times were
/// read on) is what `AGE` and `QUIET` count back from.
pub fn table(statuses: &[Status], now: u64) -> String {
    if statuses.is_empty() {
        return String::new();
    }
    let header = [
        "ID",
        "STATE",
        "PID",
        "PROGRAM",
        "DIRECTORY",
        "SIZE",
        "CLIENTS",
        "AGE",
        "QUIET",
        "LABEL",
    ];
    let mut rows: Vec<[String; 10]> = vec![header.map(str::to_owned)];
    for status in statuses {
        let unknown = status.state == State::BROKEN;
        let dash = || "-".to_owned();
        rows.push([
            printable(status.id.as_str().as_bytes()),
            state_cell(status),
            if unknown {
                dash()
            } else {
                status.pid.to_string()
            },
            status.program.as_deref().map_or_else(dash, printable),
            status.cwd.as_deref().map_or_else(dash, printable),
            if unknown {
                dash()
            } else {
                format!("{}x{}", status.size.cols, status.size.rows)
            },
            if unknown {
                dash()
            } else {
                clients_cell(status)
            },
            // How long the session has been open, and how long since the
            // program last wrote.
            ago(status.started, now),
            ago(status.output_at, now),
            // Last, because it is whatever the person wrote and the rest
            // line up before it.
            status
                .label
                .as_deref()
                .map_or_else(String::new, |label| printable(label.as_bytes())),
        ]);
    }
    let mut widths = [0usize; 10];
    for row in &rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let mut text = String::new();
    for row in &rows {
        let last = row.len() - 1;
        let mut line = String::new();
        for (i, (cell, width)) in row.iter().zip(&widths).enumerate() {
            if i == last {
                line.push_str(cell);
            } else {
                let _ = write!(line, "{cell:<width$}  ");
            }
        }
        // A session with no label leaves the last cell empty.
        text.push_str(line.trim_end());
        text.push('\n');
    }
    text
}

fn json_string(out: &mut String, bytes: &[u8]) {
    out.push('"');
    for c in String::from_utf8_lossy(bytes).chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

fn json_number(out: &mut String, number: Option<u64>) {
    match number {
        Some(number) => {
            let _ = write!(out, "{number}");
        }
        None => out.push_str("null"),
    }
}

fn json_optional(out: &mut String, bytes: Option<&[u8]>) {
    match bytes {
        Some(bytes) => json_string(out, bytes),
        None => out.push_str("null"),
    }
}

/// The sessions as a JSON array, one object per line.
pub fn json(statuses: &[Status]) -> String {
    let mut out = String::from("[");
    for (i, status) in statuses.iter().enumerate() {
        out.push_str(if i == 0 { "\n" } else { ",\n" });
        out.push_str("{\"id\":");
        json_string(&mut out, status.id.as_str().as_bytes());
        out.push_str(",\"state\":");
        json_string(&mut out, state_name(status.state).as_bytes());
        let _ = write!(out, ",\"pid\":{}", status.pid);
        out.push_str(",\"program\":");
        json_optional(&mut out, status.program.as_deref());
        out.push_str(",\"directory\":");
        json_optional(&mut out, status.cwd.as_deref());
        let _ = write!(
            out,
            ",\"cols\":{},\"rows\":{},\"xpixel\":{},\"ypixel\":{}",
            status.size.cols, status.size.rows, status.size.xpixel, status.size.ypixel
        );
        let _ = write!(
            out,
            ",\"clients\":{},\"offset\":{},\"retained\":{}",
            status.clients, status.offset, status.retained_from
        );
        out.push_str(",\"exit\":");
        match status.exit {
            None => out.push_str("null"),
            Some(Exit::Code(code)) => {
                let _ = write!(out, "{{\"code\":{code}}}");
            }
            Some(Exit::Signal(signal)) => {
                let _ = write!(out, "{{\"signal\":{signal}}}");
            }
        }
        out.push_str(",\"version\":");
        json_string(&mut out, status.version.as_bytes());
        out.push_str(",\"warden_version\":");
        json_string(&mut out, status.warden_version.as_bytes());
        // What the terminal is doing: whether it asks for a password, and where
        // an ssh or mosh in front connects to.
        let _ = write!(out, ",\"secret\":{}", status.secret);
        out.push_str(",\"remote\":");
        json_optional(&mut out, status.remote.as_deref());
        match status.remote_port {
            Some(port) => {
                let _ = write!(out, ",\"remote_port\":{port}");
            }
            None => out.push_str(",\"remote_port\":null"),
        }
        out.push_str(",\"label\":");
        json_optional(&mut out, status.label.as_deref().map(str::as_bytes));
        // When the session's program started, when the one in front did, whether
        // that one is the session's own, and when the program last wrote: Unix
        // seconds on the clock of the machine the session is on, or `null`.
        out.push_str(",\"started\":");
        json_number(&mut out, status.started);
        out.push_str(",\"front_started\":");
        json_number(&mut out, status.front_started);
        out.push_str(",\"front_is_own\":");
        out.push_str(match status.front_is_own {
            Some(true) => "true",
            Some(false) => "false",
            None => "null",
        });
        out.push_str(",\"output_at\":");
        json_number(&mut out, status.output_at);
        // Who is attached: a name per client in `clients`, in the order their
        // connections were made, `""` for one with no name. Left out when the relay does not
        // say (a relay from before the names), so that its absence is not
        // taken for clients without names.
        if says_who(status) {
            out.push_str(",\"clients_named\":[");
            for (i, name) in status.clients_named.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                json_string(
                    &mut out,
                    name.as_ref().map_or("", |n| n.as_str()).as_bytes(),
                );
            }
            out.push(']');
        }
        out.push('}');
    }
    out.push_str(if statuses.is_empty() { "]\n" } else { "\n]\n" });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use kablo_proto::SessionId;
    use kablo_proto::v1::Size;

    /// The moment the tables below count back from.
    const NOW: u64 = 1_760_100_000;

    fn status(id: &str) -> Status {
        let mut status = Status::new(SessionId::new(id).unwrap());
        status.pid = 42;
        status.program = Some(b"vim".to_vec());
        status.cwd = Some(b"/home/x".to_vec());
        status.size = Size {
            cols: 80,
            rows: 24,
            xpixel: 0,
            ypixel: 0,
        };
        status.clients = 1;
        status.version = "0.1.0".to_owned();
        status.warden_version = "0.1.0".to_owned();
        status
    }

    #[test]
    fn the_table_lines_up_and_is_empty_without_sessions() {
        assert_eq!(table(&[], NOW), "");
        let mut broken = Status::new(SessionId::new("gone-wrong").unwrap());
        broken.state = State::BROKEN;
        let mut ended = status("b");
        ended.state = State::ENDED;
        ended.exit = Some(Exit::Code(3));
        ended.started = Some(NOW - 8_040);
        let mut running = status("a");
        running.started = Some(NOW - 8_040);
        running.output_at = Some(NOW - 1_200);
        let text = table(&[running, ended, broken], NOW);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 4);
        assert!(
            lines[0].starts_with("ID          STATE           PID  PROGRAM"),
            "{text}"
        );
        assert!(
            lines[1].starts_with("a           running         42   vim"),
            "{text}"
        );
        assert!(
            lines[2].starts_with("b           ended (exit 3)  42   vim"),
            "{text}"
        );
        assert!(
            lines[3].starts_with("gone-wrong  broken          -    -"),
            "{text}"
        );
        // The label is the last column, and a line with none ends where its
        // quiet time does.
        assert!(
            lines[0].ends_with("CLIENTS  AGE     QUIET  LABEL"),
            "{text}"
        );
        assert!(lines[1].ends_with("80x24  1        2h 14m  20m"), "{text}");
        // An ended session has its age and nothing it could have been quiet since.
        assert!(lines[2].ends_with("80x24  1        2h 14m  -"), "{text}");
        assert!(lines[3].ends_with("-      -        -       -"), "{text}");
        let mut named = status("n");
        named.label = Some("build \u{1b}[2J".to_owned());
        let text = table(&[named], NOW);
        assert!(
            text.lines()
                .nth(1)
                .unwrap()
                .ends_with("  1        -    -      build ?[2J"),
            "{text}"
        );
    }

    #[test]
    fn the_clients_column_names_who_is_attached_when_the_relay_says() {
        use kablo_proto::v1::ClientName;
        let name = |text: &str| ClientName::new(text);
        let cell = |clients: u32, named: Vec<Option<ClientName>>| {
            let mut s = status("a");
            s.clients = clients;
            s.clients_named = named;
            clients_cell(&s)
        };
        assert_eq!(cell(0, vec![]), "0");
        assert_eq!(cell(1, vec![name("MacBook")]), "MacBook");
        assert_eq!(cell(2, vec![name("MacBook"), name("iMac")]), "MacBook,iMac");
        assert_eq!(cell(2, vec![name("MacBook"), None]), "MacBook+1");
        assert_eq!(cell(2, vec![None, name("iMac")]), "iMac+1");
        // Clients with no name are counted, as they were before the names.
        assert_eq!(cell(2, vec![None, None]), "2");
        // A relay from before the names counts and names no one.
        assert_eq!(cell(2, vec![]), "2");
        // What a client chose cannot write to the screen of whoever lists.
        assert_eq!(cell(1, vec![name("a\u{1b}[2Jb")]), "a?[2Jb");
        // In the table, in the column, and the label still last.
        let mut s = status("n");
        s.clients = 2;
        s.clients_named = vec![name("MacBook"), name("iMac")];
        s.label = Some("dev".to_owned());
        let text = table(&[s], NOW);
        assert!(
            text.lines().nth(1).unwrap().contains("  MacBook,iMac  "),
            "{text}"
        );
        assert!(text.lines().nth(1).unwrap().ends_with("  dev"), "{text}");
    }

    #[test]
    fn the_json_lists_who_is_attached_only_when_the_relay_says() {
        use kablo_proto::v1::ClientName;
        let mut s = status("a");
        s.clients = 2;
        // A relay from before the names: the count, and no list to mistake for
        // clients without names.
        let text = json(&[s.clone()]);
        assert!(text.contains("\"clients\":2,"), "{text}");
        assert!(!text.contains("clients_named"), "{text}");
        s.clients_named = vec![ClientName::new("Mac\"Book"), None];
        let text = json(&[s.clone()]);
        assert!(text.contains("\"clients\":2,"), "{text}");
        assert!(
            text.ends_with("\"clients_named\":[\"Mac\\\"Book\",\"\"]}\n]\n"),
            "{text}"
        );
        // Nobody attached, and a relay that names them: an empty list.
        s.clients = 0;
        s.clients_named.clear();
        assert!(json(&[s]).ends_with("\"clients_named\":[]}\n]\n"));
    }

    #[test]
    fn a_time_is_told_as_how_long_ago_it_was() {
        let ago_of = |seconds: u64| ago(Some(NOW - seconds), NOW);
        assert_eq!(ago_of(0), "0s");
        assert_eq!(ago_of(59), "59s");
        assert_eq!(ago_of(60), "1m");
        assert_eq!(ago_of(3_599), "59m");
        assert_eq!(ago_of(3_600), "1h 0m");
        assert_eq!(ago_of(8_040), "2h 14m");
        assert_eq!(ago_of(86_399), "23h 59m");
        assert_eq!(ago_of(86_400), "1d 0h");
        assert_eq!(ago_of(3 * 86_400 + 4 * 3_600 + 59), "3d 4h");
        // Not known, or after now by a second the clocks do not agree on.
        assert_eq!(ago(None, NOW), "-");
        assert_eq!(ago(Some(NOW + 1), NOW), "-");
        // No clock to count from.
        assert_eq!(ago(Some(NOW), 0), "-");
    }

    #[test]
    fn what_a_program_chose_cannot_write_to_the_screen() {
        let mut s = status("a");
        s.program = Some(b"\x1b]0;owned\x07vim".to_vec());
        s.cwd = Some(b"/tmp/\x1b[2J".to_vec());
        let text = table(&[s], NOW);
        assert!(
            !text.contains('\u{1b}') && !text.contains('\u{7}'),
            "{text:?}"
        );
        assert!(text.contains("?]0;owned?vim"), "{text:?}");
    }

    #[test]
    fn the_json_is_escaped_and_complete() {
        assert_eq!(json(&[]), "[]\n");
        let mut s = status("a");
        s.program = Some(b"a\"b\\c\n\x01".to_vec());
        s.cwd = None;
        s.exit = Some(Exit::Signal(9));
        let text = json(&[s, status("b")]);
        assert!(text.starts_with("[\n{\"id\":\"a\",\"state\":\"running\",\"pid\":42,"));
        assert!(
            text.contains("\"program\":\"a\\\"b\\\\c\\n\\u0001\""),
            "{text}"
        );
        assert!(text.contains("\"directory\":null"), "{text}");
        assert!(text.contains("\"exit\":{\"signal\":9}"), "{text}");
        assert!(text.contains("\"exit\":null"), "{text}");
        assert!(
            text.contains(
                "\"version\":\"0.1.0\",\"warden_version\":\"0.1.0\",\
                 \"secret\":false,\"remote\":null,\"remote_port\":null,\"label\":null,\
                 \"started\":null,\"front_started\":null,\"front_is_own\":null,\
                 \"output_at\":null}"
            ),
            "{text}"
        );
        // A warden that did not say its version is an empty string, as the relay's is.
        let mut unsaid = status("c");
        unsaid.warden_version.clear();
        assert!(json(&[unsaid]).contains("\"warden_version\":\"\","));
        // A password asked for, and an ssh in front that connects somewhere.
        let mut asking = status("d");
        asking.secret = true;
        asking.remote = Some(b"ada@devbox".to_vec());
        asking.remote_port = Some(2222);
        assert!(json(&[asking]).contains(
            "\"secret\":true,\"remote\":\"ada@devbox\",\"remote_port\":2222,\"label\":null,"
        ),);
        // The label a person gave, escaped like the rest.
        let mut named = status("e");
        named.label = Some("the \"build\"".to_owned());
        assert!(json(&[named]).contains("\"label\":\"the \\\"build\\\"\",\"started\":"));
        // The times a relay told, and the flag in all its three states.
        let mut timed = status("f");
        timed.started = Some(1_760_000_000);
        timed.front_started = Some(1_760_007_260);
        timed.front_is_own = Some(false);
        timed.output_at = Some(1_760_009_000);
        assert!(json(&[timed.clone()]).contains(
            "\"started\":1760000000,\"front_started\":1760007260,\
             \"front_is_own\":false,\"output_at\":1760009000}"
        ));
        timed.front_is_own = Some(true);
        assert!(json(&[timed]).contains("\"front_is_own\":true,"));
        assert!(text.ends_with("}\n]\n"), "{text}");
        assert_eq!(text.matches("{\"id\"").count(), 2);
    }

    #[test]
    fn a_state_with_no_name_is_a_number() {
        assert_eq!(state_name(State(9)), "unknown(9)");
    }
}
