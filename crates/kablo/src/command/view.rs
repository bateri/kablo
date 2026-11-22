//! `bateri view`: what a pane shows when it shows a view and not a terminal, read and driven by an
//! agent. The bateri app does it all (`kablo-bateri`'s `view` is the request); this reads the words,
//! takes the text from standard input where it is told to, and prints what the app answers: a
//! reading (cleaned of what a terminal would act on), or for `shot` the bytes of a PNG of any pane,
//! to a file or to standard output.

use std::io::{ErrorKind, Read, Write};
use std::time::Duration;

use kablo_bateri::Anchor;
use kablo_bateri::message::{Format, Reply, Request};
use kablo_bateri::view::{Action, KEYS_MOST, SCROLL_MOST, View, WAIT_MOST, Way, is_old, is_older};

use super::words::{address, clean};
use super::{Asked, Ctx, Done, Exit, Io, failure, unread};
use crate::app::Patience;

/// The words that start an action; any other first word is the pane.
const ACTIONS: [&str; 13] = [
    "command",
    "reveal",
    "click",
    "double-click",
    "fill",
    "type",
    "press",
    "scroll",
    "menu",
    "pick",
    "text",
    "wait",
    "shot",
];

/// The most of its text `fill` and `type` read from standard input: what is written is longer (a
/// byte is two hex digits) and a request may be 128 KiB.
const TEXT_BYTES: u64 = 60 * 1024;

/// How much longer than a `wait` waits the command waits for the app's answer.
const WAIT_SLACK: u64 = 5;

/// Where the picture of a `shot` goes: a file, or standard output where the word is a bare `-`.
#[derive(Debug, PartialEq, Eq)]
enum Shot {
    Stdout,
    File(String),
}

/// What the command line says, read.
#[derive(Debug, PartialEq, Eq)]
struct Asking {
    /// The pane as it was written; none where it was left out.
    pane: Option<String>,
    format: Format,
    action: Action,
    /// Where a `shot` goes.
    shot: Option<Shot>,
    /// Whether the text of the action is to be read from standard input (`-`).
    from_input: bool,
}

/// How long the app has to take the question: an app that holds a `wait` until what it waits for has
/// come, or its time is out, may take the conversation as late as that, so the command is patient
/// that long and a little more; every other action is taken at once. (Once taken, the answer is
/// waited for with no limit of its own: the limit of a `wait` is its `--for`.)
fn patience(action: &Action) -> Patience {
    let seconds = action.waits();
    if seconds == 0 {
        return Patience::STANDARD;
    }
    let answer = Duration::from_secs(u64::from(seconds) + WAIT_SLACK);
    Patience {
        answer: answer.max(Patience::STANDARD.answer),
        ..Patience::STANDARD
    }
}

fn is_action(word: &str) -> bool {
    ACTIONS.contains(&word)
}

/// The command line after `view`.
fn parse(words: &[String]) -> Result<Asking, Exit> {
    // Each word that is no option, and whether it is the bare `-` of standard input: after `--` a
    // dash is a word like any other.
    let mut positional: Vec<(&str, bool)> = Vec::new();
    let (mut json, mut full, mut gone) = (false, false, false);
    let mut seconds: Option<&str> = None;
    let mut rest = words.iter().map(String::as_str);
    while let Some(word) = rest.next() {
        match word {
            "--" => positional.extend(rest.by_ref().map(|word| (word, false))),
            "--json" => json = true,
            "--full" => full = true,
            "--gone" => gone = true,
            "--for" => {
                seconds = Some(
                    rest.next()
                        .ok_or_else(|| Exit::mistake("--for takes the seconds to wait"))?,
                );
            }
            option if option.starts_with('-') && option != "-" => {
                return Err(Exit::mistake(format!(
                    "view has no option {}: put -- before words that start with a dash",
                    clean(option)
                )));
            }
            plain => positional.push((plain, plain == "-")),
        }
    }
    let mut args = positional.into_iter().peekable();
    let pane = match args.peek() {
        Some((first, _)) if !is_action(first) => {
            let (first, _) = args.next().unwrap_or_default();
            if address(first).is_none() {
                return Err(Exit::mistake(format!(
                    "'{}' is neither a pane (name one as p and its number, p7) nor an action: {}",
                    clean(first),
                    ACTIONS.join(", ")
                )));
            }
            Some(first.to_owned())
        }
        _ => None,
    };
    let verb = args.next().map(|(word, _)| word);
    let args: Vec<(&str, bool)> = args.collect();
    let one = |what: &str| match args.as_slice() {
        [only] => Ok(*only),
        _ => Err(Exit::mistake(format!(
            "{} takes {what}, one word: put a text that has spaces in quotes",
            verb.unwrap_or("view")
        ))),
    };
    let text = |what: &str| one(what).map(|(word, _)| word.to_owned());
    let mut from_input = false;
    let mut shot = None;
    let action = match verb {
        None => {
            if !args.is_empty() {
                return Err(Exit::mistake("view takes a pane and an action, if any"));
            }
            Action::Read
        }
        Some("command") => match args.as_slice() {
            [(title, _)] => Action::Command((*title).to_owned()),
            [(target, _), (title, _)] => Action::CommandOn {
                target: (*target).to_owned(),
                title: (*title).to_owned(),
            },
            _ => {
                return Err(Exit::mistake(
                    "command takes the title of a command of the page, or the REF of an element and the title of one of its commands: put a title that has spaces in quotes",
                ));
            }
        },
        Some("reveal") => match args.as_slice() {
            [(target, _)] => Action::Reveal {
                target: (*target).to_owned(),
                index: None,
            },
            [(target, _), (row, _)] => Action::Reveal {
                target: (*target).to_owned(),
                index: Some(row_of(row)?),
            },
            _ => {
                return Err(Exit::mistake(
                    "reveal takes the REF of an element, or of a list, a table or an outline and the number of a row (from 1)",
                ));
            }
        },
        Some("click") => Action::Click(text("the REF of an element")?),
        Some("double-click") => Action::DoubleClick(text("the REF of an element")?),
        Some("menu") => Action::Menu(text("the REF of an element")?),
        Some("text") => Action::Text(text("the REF of a field or an editor")?),
        Some("pick") => Action::Pick(text("the title of a line of the menu, or its number")?),
        Some("wait") => Action::Wait {
            text: text("the text to wait for")?,
            gone,
            seconds: match seconds {
                None => None,
                Some(value) => Some(count("--for", value, WAIT_MOST)?),
            },
        },
        Some("type") => {
            let (typed, dash) = one("the text to type, or - for standard input")?;
            from_input = dash;
            Action::Type(typed.to_owned())
        }
        Some("fill") => match args.as_slice() {
            [(target, _), (typed, dash)] => {
                from_input = *dash;
                Action::Fill {
                    target: (*target).to_owned(),
                    text: (*typed).to_owned(),
                }
            }
            _ => {
                return Err(Exit::mistake(
                    "fill takes the REF of a field or an editor and the text, or - for standard input",
                ));
            }
        },
        Some("press") => {
            if args.is_empty() || args.len() > KEYS_MOST {
                return Err(Exit::mistake(format!(
                    "press takes from one to {KEYS_MOST} keys (cmd+enter)"
                )));
            }
            Action::Press(args.iter().map(|(key, _)| (*key).to_owned()).collect())
        }
        Some("scroll") => match args.as_slice() {
            [(target, _), (way, _)] | [(target, _), (way, _), _] => Action::Scroll {
                target: (*target).to_owned(),
                way: way_of(way)?,
                count: match args.get(2) {
                    None => None,
                    Some((value, _)) => Some(count("the number of screens", value, SCROLL_MOST)?),
                },
            },
            _ => {
                return Err(Exit::mistake(
                    "scroll takes the REF of a box, up, down, top or bottom, and the screens to go by",
                ));
            }
        },
        Some("shot") => {
            let (file, dash) = one("the file to write, or - for standard output")?;
            shot = Some(if dash {
                Shot::Stdout
            } else {
                Shot::File(file.to_owned())
            });
            Action::Shot { full }
        }
        Some(other) => return Err(Exit::mistake(format!("'{}' is no action", clean(other)))),
    };
    let word = verb.unwrap_or("view");
    let misplaced = |used: bool, option: &str, only: &str| {
        if used {
            Err(Exit::mistake(format!(
                "{word} takes no {option}: it is for {only}"
            )))
        } else {
            Ok(())
        }
    };
    misplaced(full && verb != Some("shot"), "--full", "shot")?;
    misplaced(gone && verb != Some("wait"), "--gone", "wait")?;
    misplaced(seconds.is_some() && verb != Some("wait"), "--for", "wait")?;
    if json && verb == Some("shot") {
        return Err(Exit::mistake("shot takes no --json: it writes a picture"));
    }
    Ok(Asking {
        pane,
        format: if json { Format::Json } else { Format::Text },
        action,
        shot,
        from_input,
    })
}

/// A whole number from 1 to `most`, written plainly.
fn count(what: &str, value: &str, most: u32) -> Result<u32, Exit> {
    value
        .parse::<u32>()
        .ok()
        .filter(|number| number.to_string() == value && (1..=most).contains(number))
        .ok_or_else(|| {
            Exit::mistake(format!(
                "{what} is a whole number from 1 to {most}, not '{}'",
                clean(value)
            ))
        })
}

/// A row from 1, written plainly.
fn row_of(value: &str) -> Result<u32, Exit> {
    value
        .parse::<u32>()
        .ok()
        .filter(|number| number.to_string() == value && *number >= 1)
        .ok_or_else(|| {
            Exit::mistake(format!(
                "a row is a whole number from 1, not '{}'",
                clean(value)
            ))
        })
}

fn way_of(word: &str) -> Result<Way, Exit> {
    [Way::Up, Way::Down, Way::Top, Way::Bottom]
        .into_iter()
        .find(|way| way.word() == word)
        .ok_or_else(|| {
            Exit::mistake(format!(
                "a scroll goes up, down, top or bottom, not '{}'",
                clean(word)
            ))
        })
}

/// The text of standard input, for `fill` and `type` (of `view` and of `browser`): all of it, a
/// last line break too, and at most `most` bytes (a multiple of 1 KiB, which the mistake says).
pub(super) fn input(most: u64) -> Result<String, Exit> {
    let mut bytes = Vec::new();
    std::io::stdin()
        .lock()
        .take(most + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| Exit::failed(format!("cannot read standard input: {e}")))?;
    if bytes.len() as u64 > most {
        return Err(Exit::mistake(format!(
            "the text on standard input is at most {} KiB",
            most / 1024
        )));
    }
    String::from_utf8(bytes).map_err(|_| Exit::mistake("the text on standard input is not text"))
}

/// Writes `bytes` whole to standard output. A reader that has gone is no mistake of this command
/// (`| head`), a write that fails for any other reason is: the picture would be cut.
fn put(out: &mut dyn Write, bytes: &[u8]) -> Done {
    match out.write_all(bytes).and_then(|()| out.flush()) {
        Err(e) if e.kind() != ErrorKind::BrokenPipe => {
            Err(Exit::failed(format!("cannot write the output: {e}")))
        }
        _ => Ok(()),
    }
}

/// `bateri view [PANE] [ACTION]...`.
pub fn view(ctx: &Ctx, io: &mut Io<'_>, words: &[String]) -> Done {
    let mut asking = parse(words)?;
    let terminal = match &asking.pane {
        Some(pane) => Anchor::address(pane),
        // The caller's own terminal, under whatever name it has: nobody wrote it.
        None => Anchor::address(&ctx.need_here("name the pane to read (view p4)")?),
    };
    let from_input = asking.from_input;
    let mut request = Request::View(View {
        terminal,
        format: asking.format,
        action: std::mem::replace(&mut asking.action, Action::Read),
    });
    // What is wrong with the request is said before standard input is waited for.
    if let Err(wrong) = request.encode() {
        return Err(failure(&wrong));
    }
    let Request::View(View { action, .. }) = &mut request else {
        return Err(unread());
    };
    if from_input {
        let text = input(TEXT_BYTES)?;
        match action {
            Action::Type(at) | Action::Fill { text: at, .. } => *at = text,
            _ => {}
        }
    }
    let patience = patience(action);
    match ctx.ask_patiently(&request, patience)? {
        Asked::Reply(Reply::Text(text)) if asking.shot.is_none() => {
            put(io.out, clean(&text).as_bytes())
        }
        Asked::Reply(Reply::Picture(png)) => match &asking.shot {
            Some(Shot::Stdout) => put(io.out, &png),
            Some(Shot::File(file)) => std::fs::write(file, &png)
                .map_err(|e| Exit::failed(format!("cannot write {}: {e}", clean(file)))),
            None => Err(unread()),
        },
        Asked::Reply(Reply::Failed(failed)) if is_old(&failed) => Err(Exit::failed(
            "this bateri does not read views yet; update bateri",
        )),
        Asked::Reply(Reply::Failed(failed)) if is_older(&failed) => Err(Exit::failed(
            "this bateri does not do that on a view yet (a command of an element, or reveal); update bateri",
        )),
        Asked::Reply(Reply::Failed(failed)) => Err(failure(&failed)),
        Asked::Reply(_) | Asked::Unreadable => Err(unread()),
        Asked::NoApp | Asked::Silent => Err(Exit::new(
            69,
            "the bateri app is not connected: it shows the views",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kablo_bateri::view::WAIT_DEFAULT;

    fn words(line: &str) -> Vec<String> {
        line.split(' ').map(str::to_owned).collect()
    }

    fn read(line: &str) -> Result<Asking, Exit> {
        parse(&words(line))
    }

    fn mistake(line: &str) -> String {
        let exit = read(line).expect_err(line);
        assert_eq!(exit.code, 64, "{line}");
        exit.text.unwrap_or_default()
    }

    #[test]
    fn the_pane_comes_first_and_is_left_out_for_the_callers_own() {
        let asking = read("p9@prod click k3f9qa --json").unwrap();
        assert_eq!(asking.pane.as_deref(), Some("p9@prod"));
        assert_eq!(asking.action, Action::Click("k3f9qa".into()));
        assert_eq!(asking.format, Format::Json);
        let asking = read("click k3f9qa").unwrap();
        assert_eq!(asking.pane, None);
        assert_eq!(read("p4").unwrap().action, Action::Read);
        assert_eq!(read("--json").unwrap().format, Format::Json);
    }

    #[test]
    fn every_action_is_read() {
        let cases = [
            ("command Restart", Action::Command("Restart".into())),
            (
                "command k3f9qa Restart",
                Action::CommandOn {
                    target: "k3f9qa".into(),
                    title: "Restart".into(),
                },
            ),
            (
                "reveal k3f9qa",
                Action::Reveal {
                    target: "k3f9qa".into(),
                    index: None,
                },
            ),
            (
                "reveal k3f9qa 248",
                Action::Reveal {
                    target: "k3f9qa".into(),
                    index: Some(248),
                },
            ),
            ("double-click k3f9qa", Action::DoubleClick("k3f9qa".into())),
            ("menu k3f9qa", Action::Menu("k3f9qa".into())),
            ("text k3f9qa", Action::Text("k3f9qa".into())),
            ("pick Copy", Action::Pick("Copy".into())),
            ("pick 2", Action::Pick("2".into())),
            (
                "fill k3f9qa hello",
                Action::Fill {
                    target: "k3f9qa".into(),
                    text: "hello".into(),
                },
            ),
            ("type abc", Action::Type("abc".into())),
            (
                "press cmd+enter tab",
                Action::Press(vec!["cmd+enter".into(), "tab".into()]),
            ),
            (
                "scroll k3f9qa down 3",
                Action::Scroll {
                    target: "k3f9qa".into(),
                    way: Way::Down,
                    count: Some(3),
                },
            ),
            (
                "scroll k3f9qa top",
                Action::Scroll {
                    target: "k3f9qa".into(),
                    way: Way::Top,
                    count: None,
                },
            ),
            (
                "wait Done --gone --for 30",
                Action::Wait {
                    text: "Done".into(),
                    gone: true,
                    seconds: Some(30),
                },
            ),
            ("shot a.png --full", Action::Shot { full: true }),
        ];
        for (line, action) in cases {
            assert_eq!(read(line).unwrap().action, action, "{line}");
        }
        assert_eq!(
            read("shot a.png").unwrap().shot,
            Some(Shot::File("a.png".into()))
        );
        assert_eq!(read("shot -").unwrap().shot, Some(Shot::Stdout));
        // After `--` a dash is a name like any other: a file called `-`.
        assert_eq!(
            read("shot -- -").unwrap().shot,
            Some(Shot::File("-".into()))
        );
    }

    #[test]
    fn a_dash_takes_the_text_from_standard_input_for_fill_and_type_only() {
        assert!(read("fill k3f9qa -").unwrap().from_input);
        assert!(read("type -").unwrap().from_input);
        assert!(!read("command -- -").unwrap().from_input);
        assert_eq!(
            read("command -- -").unwrap().action,
            Action::Command("-".into())
        );
        // After `--` a dash is a text of one character, for a fill as for a command.
        let literal = read("fill k3f9qa -- -").unwrap();
        assert!(!literal.from_input);
        assert_eq!(
            literal.action,
            Action::Fill {
                target: "k3f9qa".into(),
                text: "-".into()
            }
        );
        assert!(!read("type -- -").unwrap().from_input);
        // A reading of a text takes --json too: the app prints the text as it is.
        assert_eq!(read("text k3f9qa --json").unwrap().format, Format::Json);
        assert_eq!(
            read("fill k3f9qa -- --json").unwrap().action,
            Action::Fill {
                target: "k3f9qa".into(),
                text: "--json".into()
            }
        );
    }

    #[test]
    fn a_command_that_is_written_wrong_is_64_and_says_what_to_write() {
        for (line, says) in [
            ("p9 dance", "'dance' is no action"),
            ("dance", "neither a pane"),
            ("pp9 click k3f9qa", "neither a pane"),
            ("click", "REF of an element"),
            ("command", "command takes"),
            ("command a b c", "command takes"),
            ("reveal", "reveal takes"),
            ("reveal k3f9qa 0", "from 1"),
            ("reveal k3f9qa 07", "from 1"),
            ("reveal k3f9qa 2 3", "reveal takes"),
            ("click k3f9qa k3f9qb", "one word"),
            ("fill k3f9qa", "fill takes"),
            ("fill k3f9qa a b", "fill takes"),
            ("type", "text to type"),
            ("press", "from one to 16 keys"),
            ("scroll k3f9qa", "scroll takes"),
            ("scroll k3f9qa sideways", "up, down, top or bottom"),
            ("scroll k3f9qa up 0", "from 1 to 1000"),
            ("scroll k3f9qa up 03", "whole number"),
            ("scroll k3f9qa up 1 2", "scroll takes"),
            ("wait", "text to wait for"),
            ("wait Done --for 0", "from 1 to 120"),
            ("wait Done --for 121", "from 1 to 120"),
            ("wait Done --for ten", "whole number"),
            ("wait Done --for", "seconds to wait"),
            ("shot", "file to write"),
            ("shot a b", "one word"),
            ("shot a --json", "takes no --json"),
            ("click k3f9qa --full", "takes no --full"),
            ("click k3f9qa --gone", "takes no --gone"),
            ("click k3f9qa --for 3", "takes no --for"),
            ("click k3f9qa --force", "has no option --force"),
            ("p4 p5", "'p5' is no action"),
        ] {
            let said = mistake(line);
            assert!(said.contains(says), "{line}: {said}");
        }
        let sixteen = format!(
            "press {}",
            (0..=KEYS_MOST)
                .map(|n| format!("k{n}"))
                .collect::<Vec<_>>()
                .join(" ")
        );
        assert!(mistake(&sixteen).contains("from one to 16 keys"));
    }

    #[test]
    fn a_wait_is_waited_for_as_long_as_it_says_and_five_seconds_more() {
        let wait = |seconds| Action::Wait {
            text: "x".into(),
            gone: false,
            seconds,
        };
        let answer = |action: &Action| patience(action).answer.as_secs();
        assert_eq!(answer(&wait(Some(30))), 35);
        assert_eq!(answer(&wait(Some(WAIT_MOST))), 125);
        assert_eq!(answer(&wait(None)), u64::from(WAIT_DEFAULT) + 5);
        // A short wait is no shorter than any other question.
        assert_eq!(answer(&wait(Some(1))), Patience::STANDARD.answer.as_secs());
        assert_eq!(answer(&Action::Read), Patience::STANDARD.answer.as_secs());
        assert_eq!(
            answer(&Action::Shot { full: true }),
            Patience::STANDARD.answer.as_secs()
        );
        assert_eq!(patience(&wait(Some(30))).ask, Patience::STANDARD.ask);
    }
}
