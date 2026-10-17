//! `kablo list`: the sessions as a table for a person and as JSON for a
//! program.
//!
//! A session's program name and directory are bytes some other program chose,
//! and the table goes to a terminal: control characters are replaced before
//! anything is printed, or a directory named after an escape sequence would
//! write to the screen of whoever ran `kablo list`. The JSON is written by
//! hand (the keys are fixed and flat); keys may be added to it, none removed.

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

/// The table: a header, then a row per session; empty when there is none.
pub fn table(statuses: &[Status]) -> String {
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
        "LABEL",
    ];
    let mut rows: Vec<[String; 8]> = vec![header.map(str::to_owned)];
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
                status.clients.to_string()
            },
            // Last, because it is whatever the person wrote and the rest
            // line up before it.
            status
                .label
                .as_deref()
                .map_or_else(String::new, |label| printable(label.as_bytes())),
        ]);
    }
    let mut widths = [0usize; 8];
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
        assert_eq!(table(&[]), "");
        let mut broken = Status::new(SessionId::new("gone-wrong").unwrap());
        broken.state = State::BROKEN;
        let mut ended = status("b");
        ended.state = State::ENDED;
        ended.exit = Some(Exit::Code(3));
        let text = table(&[status("a"), ended, broken]);
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
        // clients do.
        assert!(lines[0].ends_with("CLIENTS  LABEL"), "{text}");
        assert!(lines[1].ends_with("80x24  1"), "{text}");
        let mut named = status("n");
        named.label = Some("build \u{1b}[2J".to_owned());
        let text = table(&[named]);
        assert!(
            text.lines()
                .nth(1)
                .unwrap()
                .ends_with("  1        build ?[2J"),
            "{text}"
        );
    }

    #[test]
    fn what_a_program_chose_cannot_write_to_the_screen() {
        let mut s = status("a");
        s.program = Some(b"\x1b]0;owned\x07vim".to_vec());
        s.cwd = Some(b"/tmp/\x1b[2J".to_vec());
        let text = table(&[s]);
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
                 \"secret\":false,\"remote\":null,\"remote_port\":null,\"label\":null}"
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
            "\"secret\":true,\"remote\":\"ada@devbox\",\"remote_port\":2222,\"label\":null}"
        ),);
        // The label a person gave, escaped like the rest.
        let mut named = status("e");
        named.label = Some("the \"build\"".to_owned());
        assert!(json(&[named]).contains("\"label\":\"the \\\"build\\\"\"}"));
        assert!(text.ends_with("}\n]\n"), "{text}");
        assert_eq!(text.matches("{\"id\"").count(), 2);
    }

    #[test]
    fn a_state_with_no_name_is_a_number() {
        assert_eq!(state_name(State(9)), "unknown(9)");
    }
}
