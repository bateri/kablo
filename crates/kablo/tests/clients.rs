//! Who is attached to a session: the names the clients give themselves when
//! they attach, as `status` and `kablo list` tell them, and the client that
//! takes the terminal from the others. Real sessions, over the real socket.

// Test support: a failure here is a failed test.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::thread::sleep;
use std::time::{Duration, Instant};

use common::{Bridge, Conn, Env, PATIENCE};
use kablo_proto::v1::{ClientName, ErrorCode, Message, Size, Status};

fn size(cols: u16, rows: u16) -> Size {
    Size {
        cols,
        rows,
        xpixel: 0,
        ypixel: 0,
    }
}

fn names(status: &Status) -> Vec<Option<String>> {
    status
        .clients_named
        .iter()
        .map(|name| name.map(|name| name.as_str().to_owned()))
        .collect()
}

fn named(names: &[&str]) -> Vec<Option<String>> {
    names.iter().map(|name| Some((*name).to_owned())).collect()
}

/// Asks `observer` (a connection that is not attached, so not one of the
/// clients) until the session counts `clients` of them.
fn status_with(observer: &mut Conn, clients: u32) -> Status {
    let start = Instant::now();
    loop {
        let status = observer.status();
        if status.clients == clients {
            return status;
        }
        assert!(
            start.elapsed() < PATIENCE,
            "still {} clients, not {clients}",
            status.clients
        );
        sleep(Duration::from_millis(50));
    }
}

/// The CLIENTS cell of `kablo list` for session `id`: the seventh column of
/// its row, which is the first thing after the size.
fn listed(env: &Env, id: &str) -> String {
    let said = env.kablo().arg("list").output().unwrap();
    let table = String::from_utf8_lossy(&said.stdout).into_owned();
    let row = table
        .lines()
        .find(|line| line.starts_with(id))
        .unwrap_or_else(|| panic!("no row for {id}: {table}"));
    row.split_whitespace().nth(6).unwrap().to_owned()
}

fn listed_json(env: &Env) -> String {
    let said = env.kablo().args(["list", "--json"]).output().unwrap();
    String::from_utf8_lossy(&said.stdout).into_owned()
}

#[test]
fn the_names_of_the_clients_attached_are_in_the_status_and_the_list_and_go_with_them() {
    let mut env = Env::new();
    let id = env.start("who", &["--", "/bin/sleep", "600"]);
    let mut observer = env.connect(&id);
    assert_eq!(observer.status().clients, 0);
    assert_eq!(listed(&env, &id), "0");

    let mut macbook = env.connect(&id);
    macbook.attach_named(0, "MacBook");
    let mut imac = env.connect(&id);
    imac.attach_named(0, "iMac");

    // Both, in the order they attached.
    let status = status_with(&mut observer, 2);
    assert_eq!(names(&status), named(&["MacBook", "iMac"]));
    assert_eq!(listed(&env, &id), "MacBook,iMac");
    let json = listed_json(&env);
    assert!(
        json.contains("\"clients\":2,")
            && json.contains("\"clients_named\":[\"MacBook\",\"iMac\"]"),
        "{json}"
    );

    // One lets go.
    macbook.send(kablo_proto::v1::Message::Detach);
    let status = status_with(&mut observer, 1);
    assert_eq!(names(&status), named(&["iMac"]));
    assert_eq!(listed(&env, &id), "iMac");

    // A client with no name is counted and says none; one that attaches again
    // with a name of its own is not the name it had.
    let mut plain = env.connect(&id);
    plain.attach(0);
    let status = status_with(&mut observer, 2);
    assert_eq!(names(&status), vec![Some("iMac".to_owned()), None]);
    assert_eq!(listed(&env, &id), "iMac+1");
    assert!(
        listed_json(&env).contains("\"clients_named\":[\"iMac\",\"\"]"),
        "{}",
        listed_json(&env)
    );
    imac.attach(0);
    let status = status_with(&mut observer, 2);
    assert_eq!(names(&status), vec![None, None]);
    assert_eq!(listed(&env, &id), "2");

    // A connection that goes away takes its name with it.
    drop(plain);
    drop(imac);
    let status = status_with(&mut observer, 0);
    assert_eq!(status.clients_named, Vec::<Option<ClientName>>::new());
    assert_eq!(listed(&env, &id), "0");
}

#[test]
fn a_name_that_is_too_long_is_refused_and_the_connection_goes_on() {
    use kablo_proto::v1::{ErrorCode, Message};
    let mut env = Env::new();
    let id = env.start("long", &["--", "/bin/sleep", "600"]);
    let mut conn = env.connect(&id);
    // 64 bytes of name: an attach no relay of this version reads, which is
    // written by hand because the type will not hold such a name.
    let mut payload = vec![0u8; 8];
    payload.extend([0, 80, 0, 24, 0, 0, 0, 0, 0, 64]);
    payload.extend([b'm'; 64]);
    conn.send_raw(kablo_proto::v1::kind::ATTACH, &payload);
    conn.until("an error", |c| !c.errors.is_empty());
    assert_eq!(conn.errors[0].0, ErrorCode::BAD_REQUEST);
    // It is not attached, and the connection answers what comes next.
    assert_eq!(conn.status().clients, 0);
    conn.send(Message::Ping(b"p".to_vec()));
    conn.until("a pong", |c| {
        c.replies
            .iter()
            .any(|p| matches!(p.message, Message::Pong(_)))
    });
}

#[test]
fn a_name_given_through_the_bridge_reaches_the_session() {
    let mut env = Env::new();
    let id = env.start("bridged", &["--", "/bin/sleep", "600"]);
    let mut bridge = Bridge::start(&env);
    bridge.attach_named(&id, 0, "MacBook");
    let mut observer = env.connect(&id);
    let status = status_with(&mut observer, 1);
    assert_eq!(names(&status), named(&["MacBook"]));
    // The bridge's own `status` is the one a person's `kablo list` asks.
    assert_eq!(names(&bridge.status(&id)), named(&["MacBook"]));
}

#[test]
fn a_client_that_takes_the_terminal_lets_the_others_go_and_tells_them_by_whom() {
    let mut env = Env::new();
    // `cat` writes back what it is typed (the terminal echoes it too).
    let id = env.start("take", &["--", "/bin/cat"]);
    let mut observer = env.connect(&id);
    let mut imac = env.connect(&id);
    imac.attach_named(0, "iMac");
    let mut macbook = env.connect(&id);
    macbook.attach_taking(0, size(100, 30), Some("MacBook"));

    // The one that was there is told by whom, and its connection stays.
    imac.until("to be told it was taken", |c| !c.errors.is_empty());
    assert_eq!(imac.errors, vec![(ErrorCode::TAKEN, "MacBook".to_owned())]);
    assert!(!imac.closed);
    // Only the one that took is attached now, and the size is the one it asked
    // for.
    let status = status_with(&mut observer, 1);
    assert_eq!(names(&status), named(&["MacBook"]));
    assert_eq!((status.size.cols, status.size.rows), (100, 30));

    // The output goes to the one that took, and not to the one that was let go.
    macbook.type_in("one\n");
    macbook.wait_for("one\r\n", 0);
    imac.pump(Duration::from_millis(300));
    assert!(
        !String::from_utf8_lossy(&imac.data).contains("one"),
        "{:?}",
        String::from_utf8_lossy(&imac.data)
    );

    // What the one that was let go types or resizes is refused as from any
    // connection that is not attached; it can still ask.
    imac.errors.clear();
    imac.type_in("x");
    imac.send(Message::Resize(size(50, 20)));
    imac.until("both refused", |c| c.errors.len() >= 2);
    assert!(
        imac.errors
            .iter()
            .all(|(code, _)| *code == ErrorCode::NOT_ATTACHED),
        "{:?}",
        imac.errors
    );
    let status = imac.status();
    assert_eq!(status.clients, 1);
    assert_eq!((status.size.cols, status.size.rows), (100, 30));

    // It can attach beside the one that took, which does not take it back and
    // is not told anything.
    imac.errors.clear();
    imac.attach_named(0, "iMac");
    let status = status_with(&mut observer, 2);
    // (Listed in the order the connections were made.)
    assert_eq!(names(&status), named(&["iMac", "MacBook"]));
    macbook.pump(Duration::from_millis(100));
    assert!(macbook.errors.is_empty(), "{:?}", macbook.errors);

    // And when it takes the terminal again it is the other way round; a client
    // that gives no name is told by none.
    imac.attach_taking(0, size(80, 24), None);
    macbook.until("to be told it was taken", |c| !c.errors.is_empty());
    assert_eq!(macbook.errors, vec![(ErrorCode::TAKEN, String::new())]);
    let status = status_with(&mut observer, 1);
    assert_eq!(status.clients_named, vec![None]);
    assert_eq!((status.size.cols, status.size.rows), (80, 24));
    imac.pump(Duration::from_millis(100));
    assert!(
        imac.errors.is_empty(),
        "taking does not tell the one that took"
    );
}

#[test]
fn a_connection_that_only_asks_or_waits_is_not_let_go_when_the_terminal_is_taken() {
    let mut env = Env::new();
    let id = env.start("take-wait", &["--", "/bin/cat"]);
    let mut waiter = env.connect(&id);
    waiter.send(Message::Wait);
    let mut imac = env.connect(&id);
    imac.attach_named(0, "iMac");
    let mut macbook = env.connect(&id);
    macbook.attach_taking(0, size(80, 24), Some("MacBook"));
    imac.until("to be told it was taken", |c| !c.errors.is_empty());

    // The waiter was never a client of the terminal: nothing reaches it.
    waiter.pump(Duration::from_millis(300));
    assert!(waiter.errors.is_empty(), "{:?}", waiter.errors);
    assert!(!waiter.closed && waiter.ended.is_none());
    assert_eq!(waiter.status().clients, 1);

    // It still hears the program end.
    macbook.type_in("\x04");
    waiter.until("the end", |c| c.ended.is_some());
}

#[test]
fn a_bridge_client_that_is_taken_loses_that_terminal_and_keeps_its_others() {
    let mut env = Env::new();
    let one = env.start("take-a", &["--", "/bin/cat"]);
    let two = env.start("take-b", &["--", "/bin/cat"]);
    let mut bridge = Bridge::start(&env);
    bridge.attach_named(&one, 0, "MacBook");
    bridge.attach_named(&two, 0, "MacBook");
    let mut imac = env.connect(&one);
    imac.attach_taking(0, size(80, 24), Some("iMac"));

    // The bridge passes on that its client was taken from the first terminal.
    let (session, code, text) = bridge.next_error();
    assert_eq!(
        (session.as_deref(), code, text.as_str()),
        (Some(one.as_str()), ErrorCode::TAKEN, "iMac")
    );
    // The second terminal is as it was, on the same connection.
    bridge.type_in(&two, "two\n");
    bridge.wait_for(&two, "two\r\n");
    // The first is not the client's any more: what it types there is refused,
    // and the output of the one that took does not come.
    bridge.type_in(&one, "x");
    let (session, code, _) = bridge.next_error();
    assert_eq!(
        (session.as_deref(), code),
        (Some(one.as_str()), ErrorCode::NOT_ATTACHED)
    );
    imac.type_in("mine\n");
    imac.wait_for("mine\r\n", 0);
    bridge.pump(Duration::from_millis(300));
    assert!(
        !String::from_utf8_lossy(&bridge.seen(&one).data).contains("mine"),
        "{:?}",
        String::from_utf8_lossy(&bridge.seen(&one).data)
    );
    assert_eq!(bridge.status(&two).clients, 1);
    assert_eq!(names(&bridge.status(&one)), named(&["iMac"]));
}

#[test]
fn a_bridge_client_that_was_taken_is_not_attached_again_by_a_relay_that_starts_anew() {
    let mut env = Env::new();
    let id = env.start("take-restart", &["--", "/bin/cat"]);
    let mut bridge = Bridge::start(&env);
    bridge.attach_named(&id, 0, "MacBook");
    let mut imac = env.connect(&id);
    imac.attach_taking(0, size(80, 24), Some("iMac"));
    let (_, code, _) = bridge.next_error();
    assert_eq!(code, ErrorCode::TAKEN);

    // The relay dies and the warden starts another; the bridge connects to it
    // for its next request. If it still held its attach it would attach again
    // and be listed as a client of the terminal.
    let relay = env.find("relay", &id).expect("a relay");
    common::signal(relay, libc::SIGKILL);
    let start = Instant::now();
    while env.find("relay", &id).is_none_or(|new| new == relay) {
        assert!(start.elapsed() < PATIENCE, "no new relay");
        sleep(Duration::from_millis(20));
    }
    let status = bridge.status(&id);
    assert_eq!(status.clients, 0, "{:?}", names(&status));
}
