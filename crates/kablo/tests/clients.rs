//! Who is attached to a session: the names the clients give themselves when
//! they attach, as `status` and `kablo list` tell them. Real sessions, over the
//! real socket.

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
use kablo_proto::v1::{ClientName, Status};

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
