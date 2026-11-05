//! The verbs that open no terminal: what is kablo's own (`send`, `send-key`, `capture`, `wait`,
//! `rename pane`, and `close pane` with no bateri app to ask) is run in this process, and what is about
//! the bateri app's windows is asked of the bateri app.

use std::ffi::OsString;

use kablo_bateri::message::{
    Arrange, Format, Home, MAX_STEPS, Placed, Reply, Request, Subject, TabHome, WindowHome,
};
use kablo_bateri::{Anchor, Id, IdKind, Side, paths};

use super::machine;
use super::words::{
    KINDS, address, check_id, check_label, clean, is_kind, is_terminal, reads_as_id, wrong_kind,
};
use super::{Asked, Ctx, Done, Exit, Io, failure, finish, json_name, unread};
use crate::{cli, drive};

/// The mistake of a word that is no pane: it says how to name one and lists the panes there are.
pub fn not_a_terminal(io: &mut Io<'_>, word: &str, more: &str) -> Exit {
    io.say(&format!(
        "'{}' is not a pane: name one as p and its number (p7){more}; 'bateri ls' lists them",
        clean(word)
    ));
    let mut list = Vec::new();
    cli::list(false, &mut list, &mut std::io::sink());
    for line in String::from_utf8_lossy(&list).lines().take(30) {
        let _ = writeln!(io.err, "{line}");
    }
    Exit::status(64)
}

impl Io<'_> {
    /// Whether `id` is the id of a `kind` (`pane`, `tab`, `group`, `window`), or the mistake that
    /// says what to write.
    pub fn check_id(&mut self, verb: &str, kind: &str, id: &str) -> Done {
        check_id(verb, kind, id, |word| not_a_terminal(self, word, ""))
    }
}

fn need_terminal(io: &mut Io<'_>, word: &str) -> Done {
    if is_terminal(word) {
        Ok(())
    } else {
        Err(not_a_terminal(io, word, ""))
    }
}

fn anchor_of(word: &str) -> Option<Anchor> {
    Anchor::address(word)
}

fn id_of(word: &str) -> Option<Id> {
    Id::parse(word)
}

// ─── asking the bateri app ──────────────────────────────────────────────────────

/// A request only the bateri app can carry out, since it is about its windows.
fn app_does(ctx: &Ctx, request: &Request) -> Done {
    match ctx.ask(request)? {
        Asked::Reply(Reply::Done) => Ok(()),
        Asked::Reply(Reply::Failed(failed)) => Err(failure(&failed)),
        Asked::Reply(_) | Asked::Unreadable => Err(unread()),
        Asked::NoApp | Asked::Silent => {
            Err(Exit::new(69, "the bateri app is not connected to do it"))
        }
    }
}

/// A move, which the bateri app answers with where the pane's or the tab's tab stands.
fn app_moves(ctx: &Ctx, arrange: Arrange) -> Result<Placed, Exit> {
    match ctx.ask(&Request::Arrange(arrange))? {
        Asked::Reply(Reply::Placed(placed)) => Ok(placed),
        Asked::Reply(Reply::Failed(failed)) => Err(failure(&failed)),
        Asked::Reply(_) | Asked::Unreadable => Err(unread()),
        Asked::NoApp | Asked::Silent => {
            Err(Exit::new(69, "the bateri app is not connected to do it"))
        }
    }
}

// ─── close, ls, notify ───────────────────────────────────────────────────

/// `close pane|tab|group|window [ID]`: the kind is written, never guessed (`close p9` and a bare
/// `close` are mistakes that say what to write).
pub fn close(ctx: &Ctx, io: &mut Io<'_>, words: &[String]) -> Done {
    let Some((kind, rest)) = words.split_first() else {
        return Err(Exit::mistake(
            "close names what closes: pane, tab, group or window. Did you mean: bateri close pane",
        ));
    };
    if !KINDS.iter().any(|(word, _)| word == kind) {
        return Err(wrong_kind(
            "close",
            &KINDS,
            "pane, tab, group or window",
            kind,
        ));
    }
    if kind == "pane" {
        return close_pane(ctx, io, rest);
    }
    // A tab, a group or a window belongs to the bateri app's windows: with none connected it ends with 69.
    if rest.len() > 1 {
        return Err(Exit::mistake(format!("close {kind} names one id")));
    }
    let id = rest.first().cloned().unwrap_or_default();
    if id.starts_with('-') {
        return Err(Exit::mistake(format!(
            "close {kind} takes no option '{}'",
            clean(&id)
        )));
    }
    if id.is_empty() {
        ctx.need_here(&format!("name the {kind} to close (close {kind} ID)"))?;
    } else {
        io.check_id("close", kind, &id)?;
    }
    let named = id_of(&id);
    let subject = match kind.as_str() {
        "tab" => Subject::Tab(named),
        "group" => Subject::Group(named),
        _ => Subject::Window(named),
    };
    app_does(ctx, &Request::Arrange(Arrange::Close(subject)))
}

fn close_pane(ctx: &Ctx, io: &mut Io<'_>, args: &[String]) -> Done {
    let mut force = false;
    let mut target: Option<&String> = None;
    for arg in args {
        if arg == "--force" {
            force = true;
        } else if arg.starts_with('-') {
            return Err(Exit::mistake(format!(
                "close takes no option '{}'",
                clean(arg)
            )));
        } else if target.is_some() {
            return Err(Exit::mistake("close pane names one pane"));
        } else {
            target = Some(arg);
        }
    }
    let target = match target {
        Some(target) => {
            io.check_id("close", "pane", target)?;
            target.clone()
        }
        // The caller's own terminal under whatever name it has: a target has to be a number only
        // where it is written, and nobody wrote this one.
        None => ctx.need_here("name the pane to close (close pane p4)")?,
    };
    let remembered = |state: &std::path::Path| {
        for dir in [paths::TERMINALS, paths::PLACING] {
            let _ = std::fs::remove_file(state.join(dir).join(&target));
        }
    };
    // The bateri app closes it (the pane and the terminal as one); it refuses (77) with the level it has
    // for this machine and the setting that changes it.
    let asked = match anchor_of(&target) {
        Some(anchor) => ctx.ask(&Request::Close(anchor))?,
        None => Asked::Unreadable,
    };
    match asked {
        Asked::Reply(Reply::Done) => {
            // A kill that does not wait out the grace is kablo's to do; a pane of another machine
            // is not this machine's terminal to kill, nor to forget.
            if !target.contains('@') {
                if force {
                    let _ = drive::end_session(&target, true, &mut std::io::sink());
                }
                remembered(&ctx.state);
            }
            return Ok(());
        }
        // It could not tell just now (the server's list did not come in time): the terminal is
        // this machine's own all the same.
        Asked::Reply(Reply::Failed(failed))
            if failed.kind == kablo_bateri::message::Kind::Failed => {}
        Asked::Reply(Reply::Failed(failed)) => return Err(failure(&failed)),
        _ => {}
    }
    // No bateri app to ask, or it could not tell: the terminal is this machine's own and kablo ends it. A
    // pane of another machine is the bateri app's to close, or nobody's.
    if target.contains('@') {
        return Err(Exit::new(
            69,
            format!(
                "the bateri app is not connected to close {target}: a pane of another machine is closed through it"
            ),
        ));
    }
    let code = drive::end_session(&target, force, io.err);
    if code == 0 {
        remembered(&ctx.state);
    }
    finish(code)
}

pub fn ls(ctx: &Ctx, io: &mut Io<'_>, args: &[String]) -> Done {
    let mut format = Format::Text;
    for arg in args {
        if arg == "--json" {
            format = Format::Json;
        } else {
            return Err(Exit::mistake(format!("ls takes no '{}'", clean(arg))));
        }
    }
    let mut why = "the bateri app is not connected";
    match ctx.ask(&Request::Ls(format))? {
        Asked::Reply(Reply::Text(body)) => {
            let _ = io.out.write_all(clean(&body).as_bytes());
            return Ok(());
        }
        // It could not tell just now (the server's list did not come in time): what the server has
        // is better than nothing, as it is with no bateri app.
        Asked::Reply(Reply::Failed(failed))
            if failed.kind == kablo_bateri::message::Kind::Failed =>
        {
            why = "the bateri app could not tell what is on screen";
        }
        Asked::Reply(Reply::Failed(failed)) => return Err(failure(&failed)),
        _ => {}
    }
    // No bateri app: what the server has, and what waits to be placed.
    io.say(&format!(
        "{why}: these are the panes on this server, and none says where it is"
    ));
    if format == Format::Json {
        return finish(cli::list(true, io.out, io.err));
    }
    let code = cli::list(false, io.out, io.err);
    let mut names: Vec<String> = std::fs::read_dir(ctx.state.join(paths::PLACING))
        .map(|dir| {
            dir.filter_map(Result::ok)
                .filter_map(|entry| entry.file_name().into_string().ok())
                .filter(|name| is_terminal(name))
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    let mut header = false;
    for name in names {
        let Ok(text) = std::fs::read_to_string(ctx.state.join(paths::PLACING).join(&name)) else {
            continue;
        };
        let (mut anchor, mut side, mut ratio) = (String::new(), String::new(), String::new());
        for line in text.lines() {
            let (key, value) = line.split_once(' ').unwrap_or((line, ""));
            match key {
                "anchor" => anchor = value.to_owned(),
                "side" => side = value.to_owned(),
                "ratio" => ratio = value.to_owned(),
                _ => {}
            }
        }
        if !header {
            let _ = write!(io.out, "\nWaiting for the bateri app to place:\n");
            header = true;
        }
        let share = if ratio.is_empty() {
            String::new()
        } else {
            format!(" ({ratio}%)")
        };
        let side = if side.is_empty() { "beside" } else { &side };
        let _ = writeln!(io.out, "  {name}  {side} {anchor}{share}");
    }
    finish(code)
}

pub fn notify(ctx: &Ctx, words: &[String]) -> Done {
    if words.is_empty() {
        return Err(Exit::mistake("notify takes the words to show"));
    }
    let text = clean(
        &words
            .join(" ")
            .chars()
            .map(|c| {
                if matches!(c, '\n' | '\t' | '\r') {
                    ' '
                } else {
                    c
                }
            })
            .collect::<String>(),
    );
    if text.is_empty() {
        return Err(Exit::mistake("notify takes the words to show"));
    }
    if text.len() > 1000 {
        return Err(Exit::mistake("notify shows at most 1000 bytes"));
    }
    match ctx.ask(&Request::Notify(text))? {
        Asked::Reply(Reply::Done) => Ok(()),
        Asked::Reply(Reply::Failed(failed)) => Err(failure(&failed)),
        Asked::Reply(_) | Asked::Unreadable => Err(unread()),
        Asked::NoApp | Asked::Silent => {
            Err(Exit::new(69, "the bateri app is not connected to show it"))
        }
    }
}

// ─── focus, zoom, swap, rename, resize ───────────────────────────────────

pub fn focus(ctx: &Ctx, io: &mut Io<'_>, words: &[String]) -> Done {
    let Some((kind, rest)) = words.split_first() else {
        return Err(Exit::mistake(
            "focus names what gets the keyboard: pane, tab, group or window (focus tab t3)",
        ));
    };
    if !KINDS.iter().any(|(word, _)| word == kind) {
        return Err(wrong_kind(
            "focus",
            &KINDS,
            "pane, tab, group or window",
            kind,
        ));
    }
    if rest.len() > 1 {
        return Err(Exit::mistake(format!("focus {kind} names one id")));
    }
    let mut id = rest.first().cloned().unwrap_or_default();
    if id.starts_with('-') {
        return Err(Exit::mistake(format!(
            "focus takes no option '{}'",
            clean(&id)
        )));
    }
    if id.is_empty() {
        let me = ctx.need_here(&format!("name the {kind} to focus (focus {kind} ID)"))?;
        // A pane that is the caller's own is named by whatever name it has.
        if kind == "pane" {
            id = me;
        }
    } else {
        io.check_id("focus", kind, &id)?;
    }
    let subject = match kind.as_str() {
        "pane" => Subject::Pane(anchor_of(&id)),
        "tab" => Subject::Tab(id_of(&id)),
        "group" => Subject::Group(id_of(&id)),
        _ => Subject::Window(id_of(&id)),
    };
    app_does(ctx, &Request::Arrange(Arrange::Focus(subject)))
}

pub fn zoom(ctx: &Ctx, io: &mut Io<'_>, words: &[String]) -> Done {
    if words.len() > 1 {
        return Err(Exit::mistake("zoom names one pane"));
    }
    let pane = match words.first() {
        Some(pane) => {
            io.check_id("zoom", "pane", pane)?;
            anchor_of(pane)
        }
        None => {
            ctx.need_here("name the pane to zoom (zoom p4)")?;
            None
        }
    };
    app_does(ctx, &Request::Arrange(Arrange::Zoom(pane)))
}

pub fn swap(ctx: &Ctx, io: &mut Io<'_>, words: &[String]) -> Done {
    let [first, second] = words else {
        return Err(Exit::mistake("swap names two panes (swap p4 p5)"));
    };
    io.check_id("swap", "pane", first)?;
    io.check_id("swap", "pane", second)?;
    if first == second {
        return Err(Exit::mistake("swap names two different panes"));
    }
    let (Some(pane), Some(other)) = (anchor_of(first), anchor_of(second)) else {
        return Err(unread());
    };
    app_does(ctx, &Request::Arrange(Arrange::Swap { pane, other }))
}

/// `rename pane|tab|group ID LABEL`: an empty label takes it away. A pane's label is kablo's own
/// (the bateri app is not asked); a tab's and a group's are the bateri app's.
pub fn rename(ctx: &Ctx, io: &mut Io<'_>, words: &[String]) -> Done {
    let Some((kind, rest)) = words.split_first() else {
        return Err(Exit::mistake(
            "rename names what gets a label: pane, tab or group (rename tab t3 web)",
        ));
    };
    match kind.as_str() {
        "pane" | "tab" | "group" => {}
        "window" => return Err(Exit::mistake("a window has no label")),
        _ => {
            return Err(wrong_kind(
                "rename",
                &[("pane", 'p'), ("tab", 't'), ("group", 'g')],
                "pane, tab or group",
                kind,
            ));
        }
    }
    let Some((id, label)) = rest.split_first() else {
        return Err(Exit::mistake(format!(
            "rename names the {kind} to label (rename {kind} ID LABEL)"
        )));
    };
    io.check_id("rename", kind, id)?;
    if id.contains('@') {
        return Err(Exit::mistake(format!(
            "a pane's label is kablo's own on its machine: rename the pane there ({id} is on another machine)"
        )));
    }
    if label.is_empty() {
        return Err(Exit::mistake(format!(
            "rename needs a label: rename {kind} {id} LABEL (an empty one takes it away)"
        )));
    }
    let label = label.join(" ");
    if !label.is_empty() {
        check_label("label", &label)?;
    }
    match kind.as_str() {
        "pane" => {
            return finish(drive::label(
                &[OsString::from(id), OsString::from(&label)],
                io.out,
                io.err,
            ));
        }
        "group" if label.is_empty() => {
            return Err(Exit::mistake("a group always has a label: say which"));
        }
        _ => {}
    }
    let subject = if kind == "tab" {
        Subject::Tab(id_of(id))
    } else {
        Subject::Group(id_of(id))
    };
    let label = (!label.is_empty()).then_some(label);
    app_does(ctx, &Request::Arrange(Arrange::Rename { subject, label }))
}

pub fn resize(ctx: &Ctx, io: &mut Io<'_>, words: &[String]) -> Done {
    let mut pane: Option<&String> = None;
    let mut side: Option<Side> = None;
    let mut steps = 1u8;
    let mut args = words.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--left" | "--right" | "--above" | "--below" => take_side(&mut side, arg)?,
            "--by" => {
                let value = args
                    .next()
                    .ok_or_else(|| Exit::mistake("--by takes a number of steps"))?;
                steps = steps_of(value)?;
            }
            _ if arg.starts_with("--by=") => {
                steps = steps_of(arg.strip_prefix("--by=").unwrap_or_default())?;
            }
            _ if arg.starts_with('-') => {
                return Err(Exit::mistake(format!(
                    "resize takes no option '{}'",
                    clean(arg)
                )));
            }
            _ => {
                if pane.is_some() {
                    return Err(Exit::mistake("resize names one pane"));
                }
                pane = Some(arg);
            }
        }
    }
    let Some(side) = side else {
        return Err(Exit::mistake(
            "resize needs a side: --left, --right, --above or --below",
        ));
    };
    let pane = match pane {
        Some(pane) => {
            io.check_id("resize", "pane", pane)?;
            anchor_of(pane)
        }
        None => {
            ctx.need_here("name the pane to resize (resize p4 --right)")?;
            None
        }
    };
    app_does(
        ctx,
        &Request::Arrange(Arrange::Resize { pane, side, steps }),
    )
}

/// The side flag of `move` and `resize`: one only.
fn take_side(side: &mut Option<Side>, flag: &str) -> Done {
    if side.is_some() {
        return Err(Exit::mistake("name one side"));
    }
    *side = flag.strip_prefix("--").and_then(Side::from_word);
    Ok(())
}

fn steps_of(value: &str) -> Result<u8, Exit> {
    value
        .parse::<u8>()
        .ok()
        .filter(|steps| (1..=MAX_STEPS).contains(steps) && steps.to_string() == value)
        .ok_or_else(|| Exit::mistake("--by is a number of steps from 1 to 50"))
}

// ─── move ────────────────────────────────────────────────────────────────

pub fn moving(ctx: &Ctx, io: &mut Io<'_>, words: &[String]) -> Done {
    let Some((kind, rest)) = words.split_first() else {
        return Err(Exit::mistake(
            "move names what moves: move pane p9 --beside p3 --right, or move tab t3 --group g1",
        ));
    };
    match kind.as_str() {
        "pane" => move_pane(ctx, io, rest),
        "tab" => move_tab(ctx, io, rest),
        _ => Err(wrong_kind(
            "move",
            &[("pane", 'p'), ("tab", 't')],
            "a pane or a tab",
            kind,
        )),
    }
}

/// Where a tab goes, the words of `bateri tab`, shared by `move tab` and `move pane --new-tab`.
#[derive(Default)]
struct Where {
    group: String,
    window: String,
    new_group: String,
    json: bool,
}

impl Where {
    /// Whether a group, a window or a new group was named.
    fn named(&self) -> bool {
        !(self.group.is_empty() && self.window.is_empty() && self.new_group.is_empty())
    }

    /// Whether `args` start with an option that says where a tab goes (taken), and how many words
    /// of it there are.
    fn take(&mut self, args: &[String]) -> Result<Option<usize>, Exit> {
        let Some(arg) = args.first() else {
            return Ok(None);
        };
        if arg == "--json" {
            self.json = true;
            return Ok(Some(1));
        }
        let (key, value, used) = match arg.as_str() {
            "--group" | "--window" | "--new-group" => {
                let key = arg.trim_start_matches("--");
                let Some(value) = args.get(1) else {
                    return Err(Exit::mistake(match key {
                        "group" => "--group takes a group",
                        "window" => "--window takes a window",
                        _ => "--new-group takes the name of the group",
                    }));
                };
                (key, value.as_str(), 2)
            }
            _ => match arg.split_once('=') {
                Some((key @ ("--group" | "--window" | "--new-group"), value)) => {
                    (key.trim_start_matches("--"), value, 1)
                }
                _ => return Ok(None),
            },
        };
        match key {
            "group" => {
                if !self.group.is_empty() {
                    return Err(Exit::mistake("name one group"));
                }
                self.group = value.to_owned();
            }
            "window" => {
                if !self.window.is_empty() {
                    return Err(Exit::mistake("name one window"));
                }
                self.window = value.to_owned();
            }
            _ => {
                if !self.new_group.is_empty() {
                    return Err(Exit::mistake("name one new group"));
                }
                check_label("--new-group", value)?;
                self.new_group = value.to_owned();
            }
        }
        Ok(Some(used))
    }

    /// What the place says, checked as `bateri tab` checks it.
    fn check(&self, io: &mut Io<'_>) -> Done {
        if !self.group.is_empty() && !self.window.is_empty() {
            return Err(Exit::mistake(
                "a tab goes into a group or into a window, not both",
            ));
        }
        if !self.group.is_empty() && !self.new_group.is_empty() {
            return Err(Exit::mistake(
                "a tab goes into a group or into a new group, not both",
            ));
        }
        if !self.group.is_empty() {
            io.check_id("move", "group", &self.group)?;
        }
        if !self.window.is_empty() {
            io.check_id("move", "window", &self.window)?;
        }
        Ok(())
    }
}

fn move_tab(ctx: &Ctx, io: &mut Io<'_>, args: &[String]) -> Done {
    let mut tab = String::new();
    let mut place = Where::default();
    let mut at = 0;
    while let Some(rest) = args.get(at..).filter(|rest| !rest.is_empty()) {
        if let Some(used) = place.take(rest)? {
            at += used;
            continue;
        }
        let arg = rest.first().map(String::as_str).unwrap_or_default();
        if arg == "--label" || arg.starts_with("--label=") {
            return Err(Exit::mistake(
                "move tab takes no --label: rename tab names it",
            ));
        }
        if arg.starts_with('-') {
            return Err(Exit::mistake(format!(
                "move tab takes no option '{}'",
                clean(arg)
            )));
        }
        if !tab.is_empty() {
            return Err(Exit::mistake("move names one tab"));
        }
        tab = arg.to_owned();
        at += 1;
    }
    if tab.is_empty() {
        ctx.need_here("name the tab to move (move tab t3 --group g1)")?;
    } else {
        io.check_id("move", "tab", &tab)?;
    }
    if !place.named() {
        return Err(Exit::mistake(
            "move tab needs a place: --group GROUP, --window WINDOW or --new-group NAME",
        ));
    }
    place.check(io)?;
    let to = if let Some(group) = id_of(&place.group).filter(|_| !place.group.is_empty()) {
        TabHome::Group(group)
    } else if !place.new_group.is_empty() {
        TabHome::NewGroup {
            name: place.new_group.clone(),
            window: id_of(&place.window),
        }
    } else {
        match id_of(&place.window) {
            Some(window) => TabHome::Window(window),
            None => return Err(unread()),
        }
    };
    let placed = app_moves(
        ctx,
        Arrange::MoveTab {
            tab: id_of(&tab),
            to,
        },
    )?;
    if place.json {
        let _ = writeln!(
            io.out,
            "{{\"tab\":{},\"group\":{},\"window\":{}}}",
            json_name(placed.tab),
            json_name(placed.group),
            json_name(placed.window)
        );
    } else if !place.new_group.is_empty() {
        if let Some(group) = placed.group {
            let _ = writeln!(io.out, "{group}");
        } else {
            let _ = writeln!(io.out);
        }
    }
    Ok(())
}

/// The word that names the new tab of `move pane --new-tab`. An id here is the pane written in the
/// wrong place, and taken for a name it would move another pane than the one meant.
fn new_tab_name(name: &str) -> Result<String, Exit> {
    if reads_as_id(name) {
        return Err(Exit::mistake(format!(
            "'{name}' reads as an id, not a name: the pane goes before --new-tab (move pane p9 --new-tab web)"
        )));
    }
    check_label("--new-tab", name)?;
    Ok(name.to_owned())
}

fn move_pane(ctx: &Ctx, io: &mut Io<'_>, args: &[String]) -> Done {
    let mut pane = String::new();
    let mut beside = String::new();
    let mut in_tab = String::new();
    let mut side: Option<Side> = None;
    let mut new_tab = false;
    let mut label = String::new();
    let mut place = Where::default();
    let mut at = 0;
    while let Some(rest) = args.get(at..).filter(|rest| !rest.is_empty()) {
        if let Some(used) = place.take(rest)? {
            at += used;
            continue;
        }
        let arg = rest.first().map(String::as_str).unwrap_or_default();
        let next = rest.get(1);
        at += 1;
        match arg {
            "--left" | "--right" | "--above" | "--below" => take_side(&mut side, arg)?,
            "--beside" => {
                let value =
                    next.ok_or_else(|| Exit::mistake("--beside takes the pane to go beside"))?;
                if !beside.is_empty() {
                    return Err(Exit::mistake("name one pane to go beside"));
                }
                beside = value.clone();
                at += 1;
            }
            "--tab" => {
                if !in_tab.is_empty() {
                    return Err(Exit::mistake("name one tab"));
                }
                let value = next.ok_or_else(|| Exit::mistake("--tab takes a tab"))?;
                in_tab = value.clone();
                at += 1;
            }
            "--new-tab" => {
                if new_tab {
                    return Err(Exit::mistake("name one new tab"));
                }
                new_tab = true;
                // The word after it is the new tab's name; one that starts with a dash is another
                // option (`--new-tab=-x` names it).
                if let Some(value) = next.filter(|value| !value.starts_with('-')) {
                    label = new_tab_name(value)?;
                    at += 1;
                }
            }
            "--label" => return Err(label_misplaced()),
            _ if arg.starts_with("--tab=") => {
                if !in_tab.is_empty() {
                    return Err(Exit::mistake("name one tab"));
                }
                in_tab = arg.strip_prefix("--tab=").unwrap_or_default().to_owned();
            }
            _ if arg.starts_with("--new-tab=") => {
                if new_tab {
                    return Err(Exit::mistake("name one new tab"));
                }
                new_tab = true;
                label = new_tab_name(arg.strip_prefix("--new-tab=").unwrap_or_default())?;
            }
            _ if arg.starts_with("--label=") => return Err(label_misplaced()),
            _ if arg.starts_with('-') => {
                return Err(Exit::mistake(format!(
                    "move takes no option '{}'",
                    clean(arg)
                )));
            }
            _ => {
                if !pane.is_empty() {
                    return Err(Exit::mistake(
                        "move names one pane; a new tab's name goes after --new-tab (move pane p9 --new-tab web)",
                    ));
                }
                pane = arg.to_owned();
            }
        }
    }
    if !new_tab && place.named() {
        return Err(Exit::mistake(
            "--group, --window and --new-group go with --new-tab (move pane p9 --new-tab --group g1)",
        ));
    }
    let asked = [!beside.is_empty(), !in_tab.is_empty(), new_tab]
        .iter()
        .filter(|asked| **asked)
        .count();
    match asked {
        0 => {
            return Err(Exit::mistake(
                "move needs a place: --beside PANE and a side, --tab TAB or --new-tab",
            ));
        }
        1 => {}
        _ => {
            return Err(Exit::mistake(
                "a pane goes beside another (--beside), into a tab (--tab) or into a new tab (--new-tab): one of them",
            ));
        }
    }
    if pane.is_empty() {
        // The caller's own, named by whatever name it has.
        pane = ctx.need_here("name the pane to move (move pane p4 --new-tab)")?;
    } else {
        io.check_id("move", "pane", &pane)?;
    }
    let Some(moved) = anchor_of(&pane) else {
        return Err(unread());
    };
    let arrange = if new_tab {
        if side.is_some() {
            return Err(Exit::mistake("a new tab has no side"));
        }
        place.check(io)?;
        let home = Home {
            tab: None,
            group: id_of(&place.group).filter(|_| !place.group.is_empty()),
            new_group: (!place.new_group.is_empty()).then(|| place.new_group.clone()),
            window: id_of(&place.window)
                .filter(|_| !place.window.is_empty())
                .map(WindowHome::Existing),
            label: (!label.is_empty()).then(|| label.clone()),
        };
        Arrange::PaneToNewTab { pane: moved, home }
    } else if !in_tab.is_empty() {
        io.check_id("move", "tab", &in_tab)?;
        let Some(tab) = id_of(&in_tab).filter(|id| id.kind() == IdKind::Tab) else {
            return Err(unread());
        };
        Arrange::PaneIntoTab {
            pane: moved,
            tab,
            side,
        }
    } else {
        let Some(side) = side else {
            return Err(Exit::mistake(
                "move needs a side: --left, --right, --above or --below",
            ));
        };
        io.check_id("move", "pane", &beside)?;
        if pane == beside {
            return Err(Exit::mistake("a pane cannot go beside itself"));
        }
        let Some(beside) = anchor_of(&beside) else {
            return Err(unread());
        };
        Arrange::Move {
            pane: moved,
            beside,
            side,
        }
    };
    let placed = app_moves(ctx, arrange)?;
    if place.json {
        let _ = writeln!(
            io.out,
            "{{\"pane\":\"{pane}\",\"tab\":{},\"group\":{},\"window\":{}}}",
            json_name(placed.tab),
            json_name(placed.group),
            json_name(placed.window)
        );
    } else if new_tab {
        match placed.tab {
            Some(tab) => {
                let _ = writeln!(io.out, "{tab}");
            }
            None => {
                let _ = writeln!(io.out);
            }
        }
    }
    Ok(())
}

fn label_misplaced() -> Exit {
    Exit::mistake(
        "move takes no --label: a new tab's name goes after --new-tab (move pane p9 --new-tab web); rename names what is there",
    )
}

// ─── what is kablo's own ─────────────────────────────────────────────────

/// Whether `word` is the name of a key (`send-key` takes a session before the keys).
fn is_key(word: &str) -> bool {
    let word = word.to_ascii_lowercase();
    let function = |rest: &str| {
        rest.parse::<u8>()
            .is_ok_and(|n| (1..=12).contains(&n) && n.to_string() == rest)
    };
    matches!(
        word.as_str(),
        "enter"
            | "tab"
            | "escape"
            | "backspace"
            | "space"
            | "up"
            | "down"
            | "left"
            | "right"
            | "home"
            | "end"
            | "pageup"
            | "pagedown"
            | "insert"
            | "delete"
    ) || word.contains('+')
        || word.strip_prefix('f').is_some_and(function)
}

/// The word of a command that names a terminal as an ID (never a label), which kablo would take
/// for the session of any name: the first that is no option nor the value of one.
fn first_target(io: &mut Io<'_>, words: &[String]) -> Result<bool, Exit> {
    let mut skip = false;
    for word in words {
        if skip {
            skip = false;
        } else if word == "--history" || word == "--for" {
            skip = true;
        } else if !word.starts_with('-') {
            need_terminal(io, word)?;
            return Ok(true);
        }
    }
    Ok(false)
}

/// When the first word of a command is a pane of another machine (`p9@prod`), the command is run
/// where the pane is, with the pane's plain id: its status, or `None` when the word is no such pane.
fn carry_first(ctx: &Ctx, io: &mut Io<'_>, verb: &str, words: &[String]) -> Option<Done> {
    let (id, Some(via)) = words.first().and_then(|word| address(word))? else {
        return None;
    };
    let mut carried = vec![verb.to_owned(), id.to_owned()];
    carried.extend(words.iter().skip(1).cloned());
    Some(machine::carry(ctx, io, via, &carried))
}

/// The same for `capture` and `wait`, whose pane is the first word that is no option (nor the value
/// of one).
fn carry_target(ctx: &Ctx, io: &mut Io<'_>, verb: &str, words: &[String]) -> Option<Done> {
    let mut skip = false;
    let mut via = None;
    let mut carried = vec![verb.to_owned()];
    for word in words {
        if skip {
            skip = false;
            carried.push(word.clone());
        } else if word == "--history" || word == "--for" {
            skip = true;
            carried.push(word.clone());
        } else if word.starts_with('-') {
            carried.push(word.clone());
        } else if via.is_none() {
            match address(word) {
                Some((id, Some(machine))) => {
                    via = Some(machine);
                    carried.push(id.to_owned());
                }
                _ => carried.push(word.clone()),
            }
        } else {
            carried.push(word.clone());
        }
    }
    Some(machine::carry(ctx, io, via?, &carried))
}

pub fn send(ctx: &Ctx, io: &mut Io<'_>, rest: &[OsString], words: &[String]) -> Done {
    match words {
        [first, _, ..] => {
            if let Some(done) = carry_first(ctx, io, "send", words) {
                return done;
            }
            need_terminal(io, first)?;
        }
        // Alone it would be the text, typed into this very terminal.
        [only] if is_kind('p', only) => {
            return Err(Exit::mistake(format!(
                "send needs the text: send {only} TEXT (- reads it from standard input)"
            )));
        }
        _ => {}
    }
    finish(drive::send(rest, io.err))
}

pub fn send_key(ctx: &Ctx, io: &mut Io<'_>, rest: &[OsString], words: &[String]) -> Done {
    let Some(first) = words.first() else {
        return Err(Exit::mistake("send-key takes keys to press"));
    };
    if !is_key(first) {
        if let Some(done) = carry_first(ctx, io, "send-key", words) {
            return done;
        }
        need_terminal(io, first)?;
    }
    finish(drive::send_key(rest, io.err))
}

pub fn capture(ctx: &Ctx, io: &mut Io<'_>, rest: &[OsString], words: &[String]) -> Done {
    if let Some(done) = carry_target(ctx, io, "capture", words) {
        return done;
    }
    first_target(io, words)?;
    finish(drive::capture(rest, io.out, io.err))
}

pub fn wait(ctx: &Ctx, io: &mut Io<'_>, rest: &[OsString], words: &[String]) -> Done {
    if let Some(done) = carry_target(ctx, io, "wait", words) {
        return done;
    }
    if !first_target(io, words)? {
        return Err(Exit::mistake("wait takes the pane to wait for (wait p4)"));
    }
    finish(drive::wait(rest, io.err))
}
