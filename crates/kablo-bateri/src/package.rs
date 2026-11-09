//! What kablo and the bateri app say to each other about programs (`package state`, `package
//! offer`, `package fetch`, `package key-changed`): the same text as the rest of [`crate::message`],
//! in the same version 2, written here so that the part that is about programs is read in one place.
//!
//! **Who asks.** kablo, on a machine, always: the machine decides what it has and what it wants,
//! the bateri app it is connected to only answers, and goes to the internet only when asked. Who the
//! app answers for (which account, which terminal) is on the conversation's ticket, never in these
//! lines.
//!
//! ```text
//! bateri 2                       bateri 2
//! package state                  package offer
//! target aarch64-apple-darwin    source bateri/textpen
//! installed textpen other        target aarch64-apple-darwin
//! not-here pickpen               key ed25519:59e8…=
//!                                have 1.0.0
//! ```
//!
//! and, as an answer to the `offer`, `ok offered` with its lines:
//!
//! ```text
//! bateri 2
//! ok offered
//! source bateri/textpen
//! manifest x7b22…
//! signature x6564…
//! production yes
//! ```
//!
//! * `package state` tells the app what the machine has (`installed`, the names), what the person
//!   said not to bring here (`not-here`) and which kind of machine it is (`target`, as an `app.json`
//!   names it in `files`). The answer is a plain `ok`.
//! * `package offer` asks for the newest release of `source` (`user/repo`, an address, or the bare
//!   name of a program the app follows) that this kind of machine can open and that is newer than
//!   `have`. `key` is the key approved on the machine for the program, if any, so that the app can
//!   leave out a release it knows the machine would turn away; kablo checks it again. The answer is
//!   `ok current` (nothing newer) or `ok offered`: the `source` as the app resolved it, the
//!   `app.json` and its signature, each as `x` and the hex of its bytes ([`crate::message::hex_word`]),
//!   and `production yes` when the app keeps this machine as a Production server (the line is left
//!   out otherwise, so an answer that is not for a Production machine is the same text it always
//!   was). The file is not in it. A reply is read leaving out a line it does not know, so that the
//!   app may say more later; a request is read strictly.
//! * `package fetch` asks the app to bring the file of the release (`name`, `version`, `target`,
//!   `source`) to the machine, into the directory `.<version>.part-<part>` of the program's folder.
//!   The answer is `ok brought` when it is there, and the machine checks it before it goes in.
//! * `package key-changed` tells the app that a program stopped, since its newer release came
//!   signed with another key (`name`, the `key` it came with). The answer is a plain `ok`.
//!
//! A bateri that does not know these verbs answers `error usage` with "is no verb": [`is_old`] says
//! so, and kablo tells the person to update bateri.

use crate::message::{Failure, Kind, hex_word, push, word_of_hex};

/// The most names one line of `installed` or `not-here` carries (a name is 64 bytes at most, and a
/// request is 8 KiB).
pub const NAMES_MAX: usize = 40;

/// The most an `app.json` may be (the standard's own bound, 64 KiB).
pub const MANIFEST_MAX: usize = 64 * 1024;

/// The most a signature may be (`ed25519:` and 64 bytes in base64, and a line break).
pub const SIGNATURE_MAX: usize = 128;

/// The most of a name.
const NAME_MAX: usize = 64;

/// The most of a source, the same as an address in an `app.json`.
const SOURCE_MAX: usize = 2048;

/// What kablo asks about programs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Package {
    /// What the machine has, for the app to show a person what is not there.
    State(State),
    /// The newest release of a source this machine can open.
    Offer(Offer),
    /// Bring the file of a release to the machine.
    Fetch(Fetch),
    /// A program stopped for a key.
    KeyChanged(KeyChanged),
}

/// `package state`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct State {
    pub target: String,
    /// The programs the machine has, by name.
    pub installed: Vec<String>,
    /// The programs the person said not to bring here.
    pub not_here: Vec<String>,
}

/// `package offer`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Offer {
    pub source: String,
    pub target: String,
    /// The key approved for the program on the machine.
    pub key: Option<String>,
    /// The version the machine has, as `1.2.3`: only a newer release is wanted.
    pub have: Option<String>,
}

/// `package fetch`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fetch {
    pub name: String,
    pub version: String,
    pub target: String,
    pub source: String,
    /// The `<p>` of the directory the file is brought to.
    pub part: String,
}

/// `package key-changed`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyChanged {
    pub name: String,
    /// The key the newer release came signed with.
    pub key: String,
}

/// What the app answers to an `offer` or a `fetch` (a `state` and a `key-changed` are answered
/// with the plain `ok`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answer {
    /// Nothing newer than what the machine has.
    Current,
    /// A release, with what the machine needs to judge it.
    Offered(Offered),
    /// The file is where it was asked to be.
    Brought,
}

/// `ok offered`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Offered {
    /// The source as the app resolved it: what the machine writes down as where the program came from.
    pub source: String,
    /// The `app.json`, as it was signed.
    pub manifest: String,
    /// Its signature, as it is in `app.json.sig`.
    pub signature: String,
    /// The app keeps this machine as a Production server: a machine that has no program yet leaves
    /// updating on its own off when this is the first it installs.
    pub production: bool,
}

/// Whether a failure is a bateri that does not know the `package` verbs.
pub fn is_old(failure: &Failure) -> bool {
    failure.kind == Kind::Usage && failure.text.contains("is no verb")
}

impl Package {
    /// The verb line of the request.
    pub fn verb(&self) -> &'static str {
        match self {
            Package::State(_) => "package state",
            Package::Offer(_) => "package offer",
            Package::Fetch(_) => "package fetch",
            Package::KeyChanged(_) => "package key-changed",
        }
    }

    pub(crate) fn encode_into(&self, out: &mut String) -> Result<(), Failure> {
        out.push_str(self.verb());
        out.push('\n');
        match self {
            Package::State(state) => {
                check_target(&state.target)?;
                push(out, "target", &state.target)?;
                push_names(out, "installed", &state.installed)?;
                push_names(out, "not-here", &state.not_here)?;
            }
            Package::Offer(offer) => {
                check_source(&offer.source)?;
                check_target(&offer.target)?;
                push(out, "source", &offer.source)?;
                push(out, "target", &offer.target)?;
                if let Some(key) = &offer.key {
                    check_key(key)?;
                    push(out, "key", key)?;
                }
                if let Some(have) = &offer.have {
                    check_version(have)?;
                    push(out, "have", have)?;
                }
            }
            Package::Fetch(fetch) => {
                check_name(&fetch.name)?;
                check_version(&fetch.version)?;
                check_target(&fetch.target)?;
                check_source(&fetch.source)?;
                check_part(&fetch.part)?;
                push(out, "name", &fetch.name)?;
                push(out, "version", &fetch.version)?;
                push(out, "target", &fetch.target)?;
                push(out, "source", &fetch.source)?;
                push(out, "part", &fetch.part)?;
            }
            Package::KeyChanged(changed) => {
                check_name(&changed.name)?;
                check_key(&changed.key)?;
                push(out, "name", &changed.name)?;
                push(out, "key", &changed.key)?;
            }
        }
        Ok(())
    }
}

/// The request of a `package` verb.
pub(crate) fn read<'a>(
    verb: &'a str,
    lines: impl Iterator<Item = &'a str>,
) -> Result<Package, Failure> {
    use crate::message::Fields;
    match verb {
        "package state" => {
            let fields = Fields::read(verb, lines, &["target", "installed", "not-here"])?;
            let target = fields.need("target")?;
            check_target(target)?;
            Ok(Package::State(State {
                target: target.to_owned(),
                installed: names_of("installed", fields.get("installed"))?,
                not_here: names_of("not-here", fields.get("not-here"))?,
            }))
        }
        "package offer" => {
            let fields = Fields::read(verb, lines, &["source", "target", "key", "have"])?;
            let source = fields.need("source")?;
            let target = fields.need("target")?;
            check_source(source)?;
            check_target(target)?;
            let key = fields.get("key");
            if let Some(key) = key {
                check_key(key)?;
            }
            let have = fields.get("have");
            if let Some(have) = have {
                check_version(have)?;
            }
            Ok(Package::Offer(Offer {
                source: source.to_owned(),
                target: target.to_owned(),
                key: key.map(str::to_owned),
                have: have.map(str::to_owned),
            }))
        }
        "package fetch" => {
            let fields = Fields::read(
                verb,
                lines,
                &["name", "version", "target", "source", "part"],
            )?;
            let fetch = Fetch {
                name: fields.need("name")?.to_owned(),
                version: fields.need("version")?.to_owned(),
                target: fields.need("target")?.to_owned(),
                source: fields.need("source")?.to_owned(),
                part: fields.need("part")?.to_owned(),
            };
            check_name(&fetch.name)?;
            check_version(&fetch.version)?;
            check_target(&fetch.target)?;
            check_source(&fetch.source)?;
            check_part(&fetch.part)?;
            Ok(Package::Fetch(fetch))
        }
        "package key-changed" => {
            let fields = Fields::read(verb, lines, &["name", "key"])?;
            let changed = KeyChanged {
                name: fields.need("name")?.to_owned(),
                key: fields.need("key")?.to_owned(),
            };
            check_name(&changed.name)?;
            check_key(&changed.key)?;
            Ok(Package::KeyChanged(changed))
        }
        other => Err(Failure::usage(format!("`{other}` is no package verb"))),
    }
}

impl Answer {
    pub(crate) fn encode_into(&self, out: &mut String) {
        match self {
            Answer::Current => out.push_str("ok current\n"),
            Answer::Brought => out.push_str("ok brought\n"),
            Answer::Offered(offered) => {
                out.push_str("ok offered\n");
                out.push_str(&format!("source {}\n", one_line(&offered.source)));
                out.push_str(&format!("manifest {}\n", hex_word(&offered.manifest)));
                out.push_str(&format!("signature {}\n", hex_word(&offered.signature)));
                if offered.production {
                    out.push_str("production yes\n");
                }
            }
        }
    }

    /// The answer a status (`ok current`, `ok offered`, `ok brought`) and the lines after it say.
    pub(crate) fn read(status: &str, body: &str) -> Result<Answer, Failure> {
        use crate::message::Fields;
        match status {
            "ok current" | "ok brought" if !body.is_empty() => Err(Failure::usage(format!(
                "an `{status}` reply has nothing after it"
            ))),
            "ok current" => Ok(Answer::Current),
            "ok brought" => Ok(Answer::Brought),
            "ok offered" => {
                let lines = body.strip_suffix('\n').unwrap_or(body).split('\n');
                let fields = Fields::read_ignoring_unknown(
                    "offered",
                    lines,
                    &["source", "manifest", "signature", "production"],
                )?;
                let source = fields.need("source")?;
                check_source(source)?;
                let manifest = bounded_hex("manifest", fields.need("manifest")?, MANIFEST_MAX)?;
                let signature = bounded_hex("signature", fields.need("signature")?, SIGNATURE_MAX)?;
                Ok(Answer::Offered(Offered {
                    source: source.to_owned(),
                    manifest,
                    signature,
                    production: fields.flag("production")?,
                }))
            }
            other => Err(Failure::usage(format!("`{other}` is no status"))),
        }
    }
}

/// A source on one line, whatever it held (the app builds it from what a person wrote).
fn one_line(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

fn bounded_hex(key: &str, token: &str, most: usize) -> Result<String, Failure> {
    if token.len() > 1 + 2 * most {
        return Err(Failure::usage(format!("`{key}` is over {most} bytes")));
    }
    word_of_hex(token)
}

fn push_names(out: &mut String, key: &str, names: &[String]) -> Result<(), Failure> {
    if names.is_empty() {
        return Ok(());
    }
    if names.len() > NAMES_MAX {
        return Err(Failure::usage(format!(
            "`{key}` carries at most {NAMES_MAX} names"
        )));
    }
    for name in names {
        check_name(name)?;
    }
    push(out, key, &names.join(" "))
}

fn names_of(key: &str, line: Option<&str>) -> Result<Vec<String>, Failure> {
    let Some(line) = line else {
        return Ok(Vec::new());
    };
    let names: Vec<String> = line.split(' ').map(str::to_owned).collect();
    if names.len() > NAMES_MAX {
        return Err(Failure::usage(format!(
            "`{key}` carries at most {NAMES_MAX} names"
        )));
    }
    for name in &names {
        check_name(name)?;
    }
    Ok(names)
}

/// A program's name: what an `app.json` may call one (lowercase letters, digits and `-`, starting
/// with a letter or a digit).
fn check_name(name: &str) -> Result<(), Failure> {
    let start = |b: &u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    let bytes = name.as_bytes();
    if bytes.len() <= NAME_MAX
        && bytes.first().is_some_and(start)
        && bytes.iter().all(|b| start(b) || *b == b'-')
    {
        Ok(())
    } else {
        Err(Failure::usage(format!(
            "`{}` is not a program name",
            name.escape_debug()
        )))
    }
}

/// A kind of machine: what an `app.json` names a file by.
fn check_target(target: &str) -> Result<(), Failure> {
    let fine = !target.is_empty()
        && target.len() <= NAME_MAX
        && target
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-');
    if fine {
        Ok(())
    } else {
        Err(Failure::usage(format!(
            "`{}` is not a kind of machine",
            target.escape_debug()
        )))
    }
}

fn check_source(source: &str) -> Result<(), Failure> {
    if source.is_empty() || source.len() > SOURCE_MAX || source.chars().any(char::is_control) {
        return Err(Failure::usage(format!(
            "a source is 1 to {SOURCE_MAX} bytes of one line"
        )));
    }
    Ok(())
}

/// A key as an `app.json` writes it: `ed25519:` and base64.
fn check_key(key: &str) -> Result<(), Failure> {
    let body = key.strip_prefix("ed25519:").unwrap_or("");
    let fine = !body.is_empty()
        && key.len() <= 100
        && body
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'='));
    if fine {
        Ok(())
    } else {
        Err(Failure::usage(format!(
            "`{}` is not a key",
            key.escape_debug()
        )))
    }
}

/// `major.minor.patch`, plain numbers.
fn check_version(version: &str) -> Result<(), Failure> {
    let plain = |part: &str| {
        !part.is_empty()
            && part.len() <= 9
            && part.bytes().all(|b| b.is_ascii_digit())
            && (part == "0" || !part.starts_with('0'))
    };
    let parts: Vec<&str> = version.split('.').collect();
    if parts.len() == 3 && parts.iter().all(|part| plain(part)) {
        Ok(())
    } else {
        Err(Failure::usage(format!(
            "`{}` is not a version",
            version.escape_debug()
        )))
    }
}

fn check_part(part: &str) -> Result<(), Failure> {
    let fine = !part.is_empty()
        && part.len() <= NAME_MAX
        && part
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if fine {
        Ok(())
    } else {
        Err(Failure::usage(format!(
            "`{}` is not the name of a part",
            part.escape_debug()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{Reply, Request};

    // Version 2's text for these verbs, as the first release that reads them writes it. Like the
    // `V2_*` of the other verbs they are frozen: changing the text adds a version and its own
    // constants, it does not rewrite these.
    const V2_STATE: &str = "bateri 2\npackage state\ntarget aarch64-apple-darwin\ninstalled textpen other\nnot-here pickpen\n";
    const V2_STATE_EMPTY: &str = "bateri 2\npackage state\ntarget x86_64-unknown-linux-musl\n";
    const V2_OFFER: &str = "bateri 2\npackage offer\nsource bateri/textpen\ntarget aarch64-apple-darwin\nkey ed25519:59e8IXkv6FP4tD4uJ/fvw9/pSV+1YgEpXtXQzWLwOpU=\nhave 1.0.0\n";
    const V2_OFFER_FIRST: &str = "bateri 2\npackage offer\nsource https://apps.example.test/textpen/app.json\ntarget x86_64-apple-darwin\n";
    const V2_FETCH: &str = "bateri 2\npackage fetch\nname textpen\nversion 1.1.0\ntarget aarch64-apple-darwin\nsource bateri/textpen\npart a1b2c3\n";
    const V2_KEY_CHANGED: &str = "bateri 2\npackage key-changed\nname textpen\nkey ed25519:6kpsY+KcUgq+9VB7Ey7F+ZVHdq6+vnuSQh7qaRRG0iw=\n";
    const V2_CURRENT: &str = "bateri 2\nok current\n";
    const V2_BROUGHT: &str = "bateri 2\nok brought\n";
    // `{"a":1}` and `ed25519:xx\n`.
    const V2_OFFERED: &str = "bateri 2\nok offered\nsource bateri/textpen\nmanifest x7b2261223a317d\nsignature x656432353531393a78780a\n";
    const V2_OFFERED_PRODUCTION: &str = "bateri 2\nok offered\nsource bateri/textpen\nmanifest x7b2261223a317d\nsignature x656432353531393a78780a\nproduction yes\n";

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|name| (*name).to_owned()).collect()
    }

    fn sample() -> Vec<(Request, &'static str)> {
        vec![
            (
                Request::Package(Package::State(State {
                    target: "aarch64-apple-darwin".into(),
                    installed: names(&["textpen", "other"]),
                    not_here: names(&["pickpen"]),
                })),
                V2_STATE,
            ),
            (
                Request::Package(Package::State(State {
                    target: "x86_64-unknown-linux-musl".into(),
                    installed: vec![],
                    not_here: vec![],
                })),
                V2_STATE_EMPTY,
            ),
            (
                Request::Package(Package::Offer(Offer {
                    source: "bateri/textpen".into(),
                    target: "aarch64-apple-darwin".into(),
                    key: Some("ed25519:59e8IXkv6FP4tD4uJ/fvw9/pSV+1YgEpXtXQzWLwOpU=".into()),
                    have: Some("1.0.0".into()),
                })),
                V2_OFFER,
            ),
            (
                Request::Package(Package::Offer(Offer {
                    source: "https://apps.example.test/textpen/app.json".into(),
                    target: "x86_64-apple-darwin".into(),
                    key: None,
                    have: None,
                })),
                V2_OFFER_FIRST,
            ),
            (
                Request::Package(Package::Fetch(Fetch {
                    name: "textpen".into(),
                    version: "1.1.0".into(),
                    target: "aarch64-apple-darwin".into(),
                    source: "bateri/textpen".into(),
                    part: "a1b2c3".into(),
                })),
                V2_FETCH,
            ),
            (
                Request::Package(Package::KeyChanged(KeyChanged {
                    name: "textpen".into(),
                    key: "ed25519:6kpsY+KcUgq+9VB7Ey7F+ZVHdq6+vnuSQh7qaRRG0iw=".into(),
                })),
                V2_KEY_CHANGED,
            ),
        ]
    }

    #[test]
    fn the_requests_are_written_and_read_byte_for_byte() {
        for (request, text) in sample() {
            assert_eq!(request.encode().unwrap(), text, "{request:?}");
            assert_eq!(Request::parse(text).unwrap(), request, "{text}");
        }
    }

    #[test]
    fn the_answers_are_written_and_read_byte_for_byte() {
        let offered = |production| {
            Reply::Package(Answer::Offered(Offered {
                source: "bateri/textpen".into(),
                manifest: "{\"a\":1}".into(),
                signature: "ed25519:xx\n".into(),
                production,
            }))
        };
        for (reply, text) in [
            (Reply::Package(Answer::Current), V2_CURRENT),
            (Reply::Package(Answer::Brought), V2_BROUGHT),
            (offered(false), V2_OFFERED),
            (offered(true), V2_OFFERED_PRODUCTION),
        ] {
            assert_eq!(reply.encode(), text, "{reply:?}");
            assert_eq!(Reply::parse(text).unwrap(), reply, "{text}");
        }
    }

    #[test]
    fn a_manifest_with_line_breaks_and_quotes_survives_the_line_that_frames_it() {
        let manifest =
            "{\n  \"name\": \"textpen\",\n  \"note\": \"line one\\nline two \u{e9}\"\n}\n";
        let reply = Reply::Package(Answer::Offered(Offered {
            source: "bateri/textpen".into(),
            manifest: manifest.into(),
            signature: "ed25519:abc\n".into(),
            production: false,
        }));
        let text = reply.encode();
        assert_eq!(text.lines().count(), 5, "{text}");
        assert_eq!(Reply::parse(&text).unwrap(), reply);
    }

    #[test]
    fn an_offer_without_the_production_line_is_not_for_a_production_machine() {
        // What a bateri from before the line says: the machine is no Production one.
        let Reply::Package(Answer::Offered(offered)) = Reply::parse(V2_OFFERED).unwrap() else {
            panic!("an offer");
        };
        assert!(!offered.production);
        let Reply::Package(Answer::Offered(offered)) = Reply::parse(V2_OFFERED_PRODUCTION).unwrap()
        else {
            panic!("an offer");
        };
        assert!(offered.production);
    }

    #[test]
    fn a_reply_may_say_more_than_this_reader_knows_and_a_request_may_not() {
        // A newer app's line is passed over, wherever it stands; the lines this reader knows are
        // still read, and one it knows twice is still refused.
        let more = V2_OFFERED_PRODUCTION.replace(
            "source bateri/textpen\n",
            "source bateri/textpen\nsomething-new a b c\n",
        ) + "and-another yes\n";
        let Reply::Package(Answer::Offered(offered)) = Reply::parse(&more).unwrap() else {
            panic!("an offer");
        };
        assert!(offered.production);
        assert_eq!(offered.source, "bateri/textpen");
        let twice = V2_OFFERED.to_owned() + "source again\n";
        assert!(
            Reply::parse(&twice)
                .unwrap_err()
                .text
                .contains("given twice")
        );
        let odd = V2_OFFERED.to_owned() + "production maybe\n";
        assert!(Reply::parse(&odd).unwrap_err().text.contains("yes or no"));
        // A request is not read like this: an unknown line is a mistake.
        assert!(
            Request::parse("bateri 2\npackage offer\nsource x\ntarget a\nproduction yes\n")
                .unwrap_err()
                .text
                .contains("takes no `production`")
        );
    }

    #[test]
    fn a_reply_line_that_is_only_a_word_it_does_not_know_is_passed_over_too() {
        let more = V2_OFFERED_PRODUCTION.to_owned() + "newer\n";
        let Reply::Package(Answer::Offered(offered)) = Reply::parse(&more).unwrap() else {
            panic!("an offer");
        };
        assert!(offered.production);
        // One it knows, with no value, is a mistake; and so is an empty line.
        let bare = V2_OFFERED.to_owned() + "production\n";
        assert!(
            Reply::parse(&bare)
                .unwrap_err()
                .text
                .contains("has no value")
        );
        let empty = V2_OFFERED.to_owned() + "\n";
        assert!(Reply::parse(&empty).is_err());
    }

    #[test]
    fn a_request_that_is_not_right_is_a_usage_error_that_says_which() {
        let cases = [
            ("bateri 2\npackage state\n", "needs a `target`"),
            (
                "bateri 2\npackage state\ntarget UPPER\n",
                "not a kind of machine",
            ),
            (
                "bateri 2\npackage state\ntarget a\ninstalled Textpen\n",
                "not a program name",
            ),
            (
                "bateri 2\npackage state\ntarget a\ninstalled a  b\n",
                "not a program name",
            ),
            (
                "bateri 2\npackage state\ntarget a\ntarget b\n",
                "given twice",
            ),
            (
                "bateri 2\npackage state\ntarget a\nversion 1.0.0\n",
                "takes no `version`",
            ),
            ("bateri 2\npackage offer\ntarget a\n", "needs a `source`"),
            ("bateri 2\npackage offer\nsource x\n", "needs a `target`"),
            (
                "bateri 2\npackage offer\nsource x\ntarget a\nkey rsa:abc\n",
                "not a key",
            ),
            (
                "bateri 2\npackage offer\nsource x\ntarget a\nhave 1.0\n",
                "not a version",
            ),
            (
                "bateri 2\npackage offer\nsource x\ntarget a\nhave 01.0.0\n",
                "not a version",
            ),
            (
                "bateri 2\npackage fetch\nname textpen\n",
                "needs a `version`",
            ),
            (
                "bateri 2\npackage fetch\nname textpen\nversion 1.0.0\ntarget a\nsource x\npart ../x\n",
                "not the name of a part",
            ),
            (
                "bateri 2\npackage key-changed\nname textpen\n",
                "needs a `key`",
            ),
            (
                "bateri 2\npackage key-changed\nname ../x\nkey ed25519:abc\n",
                "not a program name",
            ),
            ("bateri 2\npackage nothing\n", "is no verb"),
        ];
        for (text, said) in cases {
            let failure = Request::parse(text).unwrap_err();
            assert_eq!(failure.kind, Kind::Usage, "{text}");
            assert!(failure.text.contains(said), "{text}: {}", failure.text);
        }
    }

    #[test]
    fn a_request_that_cannot_be_read_back_is_not_written() {
        let state = |installed: Vec<String>| {
            Request::Package(Package::State(State {
                target: "aarch64-apple-darwin".into(),
                installed,
                not_here: vec![],
            }))
        };
        let many: Vec<String> = (0..=NAMES_MAX).map(|n| format!("p{n}")).collect();
        assert!(state(many).encode().unwrap_err().text.contains("at most"));
        assert!(state(names(&["a b"])).encode().is_err());
        let offer = |source: &str| {
            Request::Package(Package::Offer(Offer {
                source: source.into(),
                target: "aarch64-apple-darwin".into(),
                key: None,
                have: None,
            }))
        };
        assert!(offer("two\nlines").encode().is_err());
        assert!(offer("").encode().is_err());
        assert!(offer(&"x".repeat(2049)).encode().is_err());
        assert!(offer(&"x".repeat(2048)).encode().is_ok());
    }

    #[test]
    fn an_answer_that_is_not_right_is_not_read() {
        let manifest = hex_word(&"x".repeat(MANIFEST_MAX + 1));
        let signature = hex_word("ed25519:xx\n");
        let too_big =
            format!("bateri 2\nok offered\nsource a\nmanifest {manifest}\nsignature {signature}\n");
        let cases = [
            ("bateri 2\nok current\nsource a\n", "nothing after it"),
            ("bateri 2\nok brought\nx\n", "nothing after it"),
            ("bateri 2\nok offered\nsource a\n", "needs a `manifest`"),
            (
                "bateri 2\nok offered\nsource a\nmanifest zz\nsignature x00\n",
                "`x`",
            ),
            (
                "bateri 2\nok offered\nsource a\nmanifest x7b\nsignature x7\n",
                "even number",
            ),
            (too_big.as_str(), "over 65536 bytes"),
        ];
        for (text, said) in cases {
            let failure = Reply::parse(text).unwrap_err();
            assert!(failure.text.contains(said), "{text}: {}", failure.text);
        }
        // The biggest manifest the standard allows goes through.
        let manifest = hex_word(&"x".repeat(MANIFEST_MAX));
        let ok =
            format!("bateri 2\nok offered\nsource a\nmanifest {manifest}\nsignature {signature}\n");
        assert!(matches!(
            Reply::parse(&ok),
            Ok(Reply::Package(Answer::Offered(_)))
        ));
    }

    #[test]
    fn a_bateri_that_does_not_know_the_verbs_is_told_by_its_own_words() {
        // What the bateri before these verbs answers to `package state`: the reader's own text.
        let text = "bateri 2\nerror usage `package state` is no verb: place, ls, close, notify, ssh, relay, focus, zoom, swap, move, resize or rename\n";
        let Reply::Failed(failure) = Reply::parse(text).unwrap() else {
            panic!("not a failure");
        };
        assert!(is_old(&failure));
        assert!(!is_old(&Failure::usage("`format` is text or json")));
        assert!(!is_old(&Failure::new(Kind::Failed, "it is no verb")));
        // And this build, asked for something it does not know, says it the same way.
        let asked = Request::parse("bateri 2\npackage nothing\n").unwrap_err();
        assert!(is_old(&asked));
    }

    #[test]
    fn every_package_request_fits_the_size_of_a_request() {
        let state = Request::Package(Package::State(State {
            target: "x86_64-unknown-linux-musl".into(),
            installed: (0..NAMES_MAX).map(|_| "a".repeat(64)).collect(),
            not_here: (0..NAMES_MAX).map(|_| "b".repeat(64)).collect(),
        }));
        let text = state.encode().unwrap();
        assert!(text.len() <= crate::message::MAX_BYTES, "{}", text.len());
        assert_eq!(Request::parse(&text).unwrap(), state);
    }
}
