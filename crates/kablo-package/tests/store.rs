//! The folder of programs, tried through the public surface on a home of its own.
//!
//! The releases in `fixture/` are `app.json` files and signatures made once by a throwaway program
//! with two keys whose seeds are `[7; 32]` (A, the author) and `[9; 32]` (B, another): the private
//! halves are worth nothing and nothing here needs them. Each lists a file per kind of machine, whose
//! body is `body(name, version, target)` and whose sum is the `app.json`'s.

// Test support: a failure here is a failed test.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime};

use kablo_package::{Error, Listing, Outcome, Put, Store, Trust, Version};

const TARGET: &str = "aarch64-apple-darwin";
const OTHER_TARGET: &str = "x86_64-unknown-linux-musl";
const DAY: Duration = Duration::from_secs(24 * 60 * 60);

static SERIAL: AtomicU32 = AtomicU32::new(0);

/// A home that is made for one test and taken away with it.
struct Home {
    dir: PathBuf,
}

impl Home {
    fn new() -> Home {
        let n = SERIAL.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("kp{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Home { dir }
    }

    /// Trusts A without being asked, as kablo trusts bateri's key.
    fn store(&self) -> Store {
        Store::new(self.dir.join("apps"), &[&key("a")])
    }

    /// Trusts nobody.
    fn strict(&self) -> Store {
        Store::new(self.dir.join("apps"), &[])
    }

    fn apps(&self) -> PathBuf {
        self.dir.join("apps")
    }

    /// Every path in the folder with what it holds: a file's bytes, a link's text.
    fn tree(&self) -> Vec<String> {
        let mut lines = Vec::new();
        walk(&self.apps(), &self.apps(), &mut lines);
        lines.sort();
        lines
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn walk(root: &Path, dir: &Path, lines: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries {
        let path = entry.unwrap().path();
        let rel = path.strip_prefix(root).unwrap().display().to_string();
        let meta = fs::symlink_metadata(&path).unwrap();
        if meta.file_type().is_symlink() {
            lines.push(format!(
                "{rel} -> {}",
                fs::read_link(&path).unwrap().display()
            ));
        } else if meta.is_dir() {
            lines.push(format!("{rel}/"));
            walk(root, &path, lines);
        } else {
            lines.push(format!("{rel} = {:?}", fs::read(&path).unwrap()));
        }
    }
}

fn fixture(file: &str) -> Vec<u8> {
    fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixture")
            .join(file),
    )
    .unwrap()
}

fn key(which: &str) -> String {
    String::from_utf8(fixture(&format!("key-{which}.pub")))
        .unwrap()
        .trim()
        .to_string()
}

fn body(name: &str, version: &str, target: &str) -> String {
    format!("#!/bin/sh\necho {name} {version} {target}\n")
}

/// A release of the fixtures: its `app.json`, its signature, and the id of the fixture.
struct Release {
    name: &'static str,
    version: &'static str,
    id: &'static str,
}

const T090: Release = Release {
    name: "textpen",
    version: "0.9.0",
    id: "textpen-0.9.0",
};
const T100: Release = Release {
    name: "textpen",
    version: "1.0.0",
    id: "textpen-1.0.0",
};
const T110: Release = Release {
    name: "textpen",
    version: "1.1.0",
    id: "textpen-1.1.0",
};
const T120: Release = Release {
    name: "textpen",
    version: "1.2.0",
    id: "textpen-1.2.0",
};
const T130_B: Release = Release {
    name: "textpen",
    version: "1.3.0",
    id: "textpen-1.3.0-other-key",
};
const OTHER: Release = Release {
    name: "other",
    version: "1.0.0",
    id: "other-1.0.0",
};

impl Release {
    fn manifest(&self) -> Vec<u8> {
        fixture(&format!("{}.app.json", self.id))
    }

    fn signature(&self) -> Vec<u8> {
        fixture(&format!("{}.app.json.sig", self.id))
    }

    /// Places the release's file for `target` the way bateri does: a directory beside the
    /// versions, with the program in it under its name.
    fn place(&self, home: &Home, part: &str, target: &str) -> PathBuf {
        self.place_body(home, part, &body(self.name, self.version, target))
    }

    fn place_body(&self, home: &Home, part: &str, content: &str) -> PathBuf {
        let dir = home
            .apps()
            .join(self.name)
            .join(format!(".{}.part-{part}", self.version));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(self.name), content).unwrap();
        dir
    }
}

fn put(store: &Store, release: &Release, part: &str, trust: Trust<'_>) -> Result<Outcome, Error> {
    let manifest = release.manifest();
    let signature = release.signature();
    store.put(&Put {
        name: release.name,
        part,
        manifest: &manifest,
        signature: &signature,
        target: TARGET,
        source: Some("bateri/textpen"),
        trust,
    })
}

/// Places the release and puts it in.
fn install(home: &Home, release: &Release) -> Result<Outcome, Error> {
    release.place(home, "t", TARGET);
    put(&home.store(), release, "t", Trust::Recorded)
}

fn v(text: &str) -> Version {
    Version::parse(text).unwrap()
}

fn age(path: &Path, by: Duration) {
    let file = fs::File::open(path).unwrap();
    file.set_modified(SystemTime::now() - by).unwrap();
}

fn link_of(home: &Home, name: &str) -> String {
    fs::read_link(home.apps().join("bin").join(name))
        .unwrap()
        .display()
        .to_string()
}

#[test]
fn a_release_signed_by_a_key_kablo_ships_with_is_put_in_and_linked() {
    let home = Home::new();
    let done = install(&home, &T100).unwrap();
    assert_eq!(
        done,
        Outcome::Installed {
            version: v("1.0.0"),
            before: None
        }
    );
    let dir = home.apps().join("textpen/1.0.0");
    assert_eq!(link_of(&home, "textpen"), "../textpen/1.0.0/textpen");
    assert_eq!(
        fs::read_to_string(dir.join("textpen")).unwrap(),
        body("textpen", "1.0.0", TARGET)
    );
    assert_eq!(fs::read(dir.join("app.json")).unwrap(), T100.manifest());
    assert_eq!(
        fs::read(dir.join("app.json.sig")).unwrap(),
        T100.signature()
    );
    assert_eq!(
        fs::metadata(dir.join("textpen"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o755
    );
    // The link reaches the program, and the staging place is gone.
    let reached = fs::read_to_string(home.apps().join("bin/textpen")).unwrap();
    assert_eq!(reached, body("textpen", "1.0.0", TARGET));
    assert!(!home.apps().join("textpen/.1.0.0.part-t").exists());
    let listing = home.store().list().unwrap();
    assert_eq!(listing.programs.len(), 1);
    assert_eq!(
        listing.programs[0].source.as_deref(),
        Some("bateri/textpen")
    );
    assert_eq!(listing.programs[0].key, Some(key("a")));
}

#[test]
fn the_file_taken_is_the_one_for_this_kind_of_machine() {
    let home = Home::new();
    T100.place(&home, "t", OTHER_TARGET);
    // The sum is that of the file for TARGET, so a file for another machine is damaged.
    let error = put(&home.store(), &T100, "t", Trust::Recorded).unwrap_err();
    assert!(matches!(error, Error::Damaged), "{error}");
    assert!(!home.apps().join("bin/textpen").exists());
}

#[test]
fn a_version_that_is_there_is_not_put_in_again() {
    let home = Home::new();
    install(&home, &T100).unwrap();
    let before = home.tree();
    T100.place(&home, "again", TARGET);
    let done = put(&home.store(), &T100, "again", Trust::Recorded).unwrap();
    assert_eq!(
        done,
        Outcome::Unchanged {
            current: v("1.0.0")
        }
    );
    assert_eq!(home.tree(), before);
}

#[test]
fn a_newer_release_takes_the_link_and_the_older_one_stays() {
    let home = Home::new();
    install(&home, &T100).unwrap();
    let done = install(&home, &T110).unwrap();
    assert_eq!(
        done,
        Outcome::Installed {
            version: v("1.1.0"),
            before: Some(v("1.0.0"))
        }
    );
    assert_eq!(link_of(&home, "textpen"), "../textpen/1.1.0/textpen");
    assert!(home.apps().join("textpen/1.0.0/textpen").is_file());
}

#[test]
fn an_older_release_is_refused_and_the_folder_is_as_it_was() {
    let home = Home::new();
    install(&home, &T110).unwrap();
    let before = home.tree();
    T100.place(&home, "t", TARGET);
    let error = put(&home.store(), &T100, "t", Trust::Recorded).unwrap_err();
    assert!(
        matches!(error, Error::Older { current, offered } if current == v("1.1.0") && offered == v("1.0.0")),
        "{error}"
    );
    // What was placed is taken away; nothing else changed.
    assert_eq!(home.tree(), before);
}

#[test]
fn a_file_that_is_not_the_one_signed_is_refused_and_the_folder_is_as_it_was() {
    let home = Home::new();
    install(&home, &T100).unwrap();
    let before = home.tree();
    T110.place_body(&home, "t", "#!/bin/sh\necho not what was signed\n");
    let error = put(&home.store(), &T110, "t", Trust::Recorded).unwrap_err();
    assert!(matches!(error, Error::Damaged), "{error}");
    assert_eq!(home.tree(), before);
    assert_eq!(link_of(&home, "textpen"), "../textpen/1.0.0/textpen");
}

#[test]
fn a_half_copied_file_is_refused_and_the_link_does_not_move() {
    let home = Home::new();
    install(&home, &T100).unwrap();
    let whole = body("textpen", "1.1.0", TARGET);
    T110.place_body(&home, "t", &whole[..whole.len() / 2]);
    let error = put(&home.store(), &T110, "t", Trust::Recorded).unwrap_err();
    assert!(matches!(error, Error::Damaged), "{error}");
    assert_eq!(link_of(&home, "textpen"), "../textpen/1.0.0/textpen");
    assert!(!home.apps().join("textpen/1.1.0").exists());
}

#[test]
fn a_release_with_no_file_placed_is_refused() {
    let home = Home::new();
    fs::create_dir_all(home.apps().join("textpen/.1.0.0.part-t")).unwrap();
    let error = put(&home.store(), &T100, "t", Trust::Recorded).unwrap_err();
    assert!(matches!(error, Error::Missing(_)), "{error}");
    assert!(!home.apps().join("bin").exists());
}

#[test]
fn a_file_that_is_a_link_is_not_read() {
    let home = Home::new();
    let dir = home.apps().join("textpen/.1.0.0.part-t");
    fs::create_dir_all(&dir).unwrap();
    let real = home.dir.join("elsewhere");
    fs::write(&real, body("textpen", "1.0.0", TARGET)).unwrap();
    symlink(&real, dir.join("textpen")).unwrap();
    let error = put(&home.store(), &T100, "t", Trust::Recorded).unwrap_err();
    assert!(matches!(error, Error::Missing(_)), "{error}");
}

#[test]
fn a_key_nobody_approved_is_refused_unless_a_person_approves_it_now() {
    let home = Home::new();
    T100.place(&home, "t", TARGET);
    let error = put(&home.strict(), &T100, "t", Trust::Recorded).unwrap_err();
    assert!(
        matches!(&error, Error::Unapproved { key: k } if *k == key("a")),
        "{error}"
    );
    assert!(!home.apps().join("textpen/1.0.0").exists());

    // The approval is for this key and no other.
    T100.place(&home, "t", TARGET);
    let error = put(&home.strict(), &T100, "t", Trust::Approved(&key("b"))).unwrap_err();
    assert!(matches!(error, Error::Unapproved { .. }), "{error}");

    T100.place(&home, "t", TARGET);
    put(&home.strict(), &T100, "t", Trust::Approved(&key("a"))).unwrap();
    assert_eq!(home.strict().key("textpen").unwrap(), Some(key("a")));
    // From then on the program's own key is enough.
    install_with(&home.strict(), &home, &T110).unwrap();
}

fn install_with(store: &Store, home: &Home, release: &Release) -> Result<Outcome, Error> {
    release.place(home, "t", TARGET);
    put(store, release, "t", Trust::Recorded)
}

#[test]
fn a_release_signed_with_another_key_is_refused_and_the_folder_is_as_it_was() {
    let home = Home::new();
    install(&home, &T100).unwrap();
    let before = home.tree();
    T130_B.place(&home, "t", TARGET);
    let error = put(&home.store(), &T130_B, "t", Trust::Recorded).unwrap_err();
    assert!(
        matches!(&error, Error::KeyChanged { recorded, offered } if *recorded == key("a") && *offered == key("b")),
        "{error}"
    );
    assert_eq!(home.tree(), before);
    // The approval of another key is not the approval of this one.
    T130_B.place(&home, "t", TARGET);
    let error = put(&home.store(), &T130_B, "t", Trust::Approved(&key("a"))).unwrap_err();
    assert!(matches!(error, Error::KeyChanged { .. }), "{error}");
}

#[test]
fn a_person_can_approve_the_new_key_and_it_takes_the_place_of_the_old() {
    let home = Home::new();
    install(&home, &T100).unwrap();
    assert!(home.store().stop("textpen", &key("b")).unwrap());
    T130_B.place(&home, "t", TARGET);
    let done = put(&home.store(), &T130_B, "t", Trust::Approved(&key("b"))).unwrap();
    assert!(matches!(done, Outcome::Installed { .. }));
    assert_eq!(home.store().key("textpen").unwrap(), Some(key("b")));
    // The program is no longer stopped.
    assert_eq!(home.store().stopped("textpen").unwrap(), None);
    assert_eq!(link_of(&home, "textpen"), "../textpen/1.3.0/textpen");
}

#[test]
fn a_release_that_was_changed_after_it_was_signed_is_refused() {
    let home = Home::new();
    T100.place(&home, "t", TARGET);
    let mut manifest = T100.manifest();
    let at = manifest.iter().position(|b| *b == b'1').unwrap();
    manifest[at] = b'2';
    let error = home
        .store()
        .put(&Put {
            name: "textpen",
            part: "t",
            manifest: &manifest,
            signature: &T100.signature(),
            target: TARGET,
            source: None,
            trust: Trust::Recorded,
        })
        .unwrap_err();
    assert!(matches!(error, Error::Manifest(_)), "{error}");
    assert!(!home.apps().join("bin").exists());
}

#[test]
fn the_release_of_another_program_is_not_this_ones() {
    let home = Home::new();
    OTHER.place(&home, "t", TARGET);
    let manifest = OTHER.manifest();
    let signature = OTHER.signature();
    let error = home
        .store()
        .put(&Put {
            name: "textpen",
            part: "t",
            manifest: &manifest,
            signature: &signature,
            target: TARGET,
            source: None,
            trust: Trust::Recorded,
        })
        .unwrap_err();
    assert!(
        matches!(&error, Error::WrongName { asked, signed } if asked == "textpen" && signed == "other"),
        "{error}"
    );
}

#[test]
fn a_release_with_no_file_for_this_kind_of_machine_is_refused() {
    let home = Home::new();
    T100.place(&home, "t", "riscv64");
    let manifest = T100.manifest();
    let signature = T100.signature();
    let error = home
        .store()
        .put(&Put {
            name: "textpen",
            part: "t",
            manifest: &manifest,
            signature: &signature,
            target: "riscv64",
            source: None,
            trust: Trust::Recorded,
        })
        .unwrap_err();
    assert!(matches!(error, Error::NoFile(_)), "{error}");
}

#[test]
fn a_name_that_is_the_machines_own_is_not_a_program() {
    let home = Home::new();
    let store = home.store();
    for name in ["bin", "auto-off", "not-here", "kablo", "bateri", "pencere"] {
        assert!(
            matches!(store.remove(name), Err(Error::Reserved(_))),
            "{name}"
        );
        assert!(matches!(store.key(name), Err(Error::Reserved(_))), "{name}");
    }
    for name in ["", "..", "../x", "a/b", ".hidden", "Upper", "a b", "-dash"] {
        assert!(
            matches!(store.remove(name), Err(Error::Name(_))),
            "{name:?}"
        );
    }
    let long = "a".repeat(65);
    assert!(matches!(store.remove(&long), Err(Error::Name(_))));
    let error = store
        .put(&Put {
            name: "bin",
            part: "t",
            manifest: b"",
            signature: b"",
            target: TARGET,
            source: None,
            trust: Trust::Recorded,
        })
        .unwrap_err();
    assert!(matches!(error, Error::Reserved(_)), "{error}");
}

#[test]
fn a_part_name_cannot_leave_the_programs_folder() {
    let home = Home::new();
    for part in ["", "../x", "a/b", "a b", ".."] {
        let error = put(&home.store(), &T100, part, Trust::Recorded).unwrap_err();
        assert!(matches!(error, Error::Part(_)), "{part:?}: {error}");
    }
}

#[test]
fn a_link_that_is_not_kablos_is_left_as_it_is() {
    let home = Home::new();
    fs::create_dir_all(home.apps().join("bin")).unwrap();
    fs::write(home.apps().join("bin/textpen"), "mine").unwrap();
    T100.place(&home, "t", TARGET);
    let error = put(&home.store(), &T100, "t", Trust::Recorded).unwrap_err();
    assert!(matches!(error, Error::Foreign(_)), "{error}");
    assert_eq!(
        fs::read_to_string(home.apps().join("bin/textpen")).unwrap(),
        "mine"
    );
    assert!(!home.apps().join("textpen/1.0.0").exists());
    // Removing the program does not remove it either.
    home.store().remove("textpen").unwrap();
    assert_eq!(
        fs::read_to_string(home.apps().join("bin/textpen")).unwrap(),
        "mine"
    );
}

#[test]
fn a_version_put_in_but_never_linked_is_linked_by_the_next_try() {
    let home = Home::new();
    install(&home, &T100).unwrap();
    // As if a run stopped after the rename: the directory is there, the link is the old one.
    fs::create_dir_all(home.apps().join("textpen/1.1.0")).unwrap();
    fs::write(
        home.apps().join("textpen/1.1.0/textpen"),
        body("textpen", "1.1.0", TARGET),
    )
    .unwrap();
    assert_eq!(home.store().current("textpen").unwrap(), Some(v("1.0.0")));
    let done = install(&home, &T110).unwrap();
    assert!(matches!(done, Outcome::Installed { version, .. } if version == v("1.1.0")));
    assert_eq!(link_of(&home, "textpen"), "../textpen/1.1.0/textpen");
    assert!(!home.apps().join("textpen/.1.1.0.part-t").exists());
}

#[test]
fn what_a_machine_keeps_is_the_linked_version_the_one_before_it_and_the_young() {
    let home = Home::new();
    let app = home.apps().join("textpen");
    install(&home, &T090).unwrap();
    install(&home, &T100).unwrap();
    install(&home, &T110).unwrap();
    // Nothing is old yet: everything stays.
    for version in ["0.9.0", "1.0.0", "1.1.0"] {
        assert!(app.join(version).is_dir(), "{version}");
    }
    age(&app.join("0.9.0"), 40 * DAY);
    age(&app.join("1.0.0"), 31 * DAY);
    age(&app.join("1.1.0"), 35 * DAY);
    fs::create_dir_all(app.join("0.1.0")).unwrap();
    age(&app.join("0.1.0"), 29 * DAY);
    fs::create_dir_all(app.join("1.9.0")).unwrap();
    age(&app.join("1.9.0"), 90 * DAY);
    // A leftover of a release that never ended, and one that is under way.
    fs::create_dir_all(app.join(".0.5.0.part-gone")).unwrap();
    age(&app.join(".0.5.0.part-gone"), 2 * DAY);
    fs::create_dir_all(app.join(".0.6.0.part-live")).unwrap();

    install(&home, &T120).unwrap();
    assert!(!app.join("0.9.0").exists(), "old and not the one before");
    assert!(!app.join("1.0.0").exists(), "old and not the one before");
    assert!(
        app.join("1.1.0").is_dir(),
        "the one before the linked is kept however old"
    );
    assert!(app.join("1.2.0").is_dir());
    assert!(app.join("0.1.0").is_dir(), "younger than thirty days");
    assert!(
        !app.join("1.9.0").exists(),
        "old, and neither the linked nor the one before it"
    );
    assert!(!app.join(".0.5.0.part-gone").exists());
    assert!(app.join(".0.6.0.part-live").is_dir());
    assert_eq!(link_of(&home, "textpen"), "../textpen/1.2.0/textpen");
}

#[test]
fn remove_takes_the_link_and_everything_the_program_has() {
    let home = Home::new();
    install(&home, &T100).unwrap();
    install(&home, &T110).unwrap();
    assert!(home.store().remove("textpen").unwrap());
    assert!(!home.apps().join("textpen").exists());
    assert!(fs::symlink_metadata(home.apps().join("bin/textpen")).is_err());
    assert!(!home.store().remove("textpen").unwrap());
    assert!(home.store().list().unwrap().programs.is_empty());
}

#[test]
fn the_listing_is_the_folder() {
    let home = Home::new();
    assert_eq!(
        home.store().list().unwrap(),
        Listing {
            programs: vec![],
            auto: true,
            not_here: vec![]
        }
    );
    install(&home, &T100).unwrap();
    home.store().stop("textpen", &key("b")).unwrap();
    home.store().set_auto(false).unwrap();
    home.store()
        .mark_not_here(&["zed", "alpha", "zed"])
        .unwrap();
    // A folder that has no version linked is not a program that is here.
    fs::create_dir_all(home.apps().join("leftover/1.0.0")).unwrap();
    let listing = home.store().list().unwrap();
    assert!(!listing.auto);
    assert_eq!(listing.not_here, ["alpha", "zed"]);
    assert_eq!(listing.programs.len(), 1);
    let program = &listing.programs[0];
    assert_eq!(program.name, "textpen");
    assert_eq!(program.version, v("1.0.0"));
    assert_eq!(program.stopped, Some(key("b")));
    assert_eq!(home.store().names().unwrap(), ["textpen"]);
}

#[test]
fn automatic_updating_is_on_until_it_is_turned_off() {
    let home = Home::new();
    let store = home.store();
    assert!(store.auto());
    store.set_auto(true).unwrap();
    store.set_auto(false).unwrap();
    assert!(!store.auto());
    assert!(home.apps().join("auto-off").is_file());
    store.set_auto(false).unwrap();
    store.set_auto(true).unwrap();
    assert!(store.auto());
}

#[test]
fn the_names_not_to_bring_here_are_kept_apart_and_an_install_clears_its_own() {
    let home = Home::new();
    let store = home.store();
    assert_eq!(store.not_here().unwrap(), Vec::<String>::new());
    store.mark_not_here(&["textpen", "other"]).unwrap();
    assert_eq!(store.not_here().unwrap(), ["other", "textpen"]);
    assert!(store.mark_not_here(&["ok", "Not A Name"]).is_err());
    assert_eq!(store.not_here().unwrap(), ["other", "textpen"]);
    install(&home, &T100).unwrap();
    assert_eq!(store.not_here().unwrap(), ["other"]);
}

#[test]
fn a_program_is_stopped_once_per_key() {
    let home = Home::new();
    let store = home.store();
    assert!(matches!(
        store.stop("textpen", &key("b")),
        Err(Error::NotInstalled(_))
    ));
    install(&home, &T100).unwrap();
    assert!(store.stop("textpen", &key("b")).unwrap());
    assert!(!store.stop("textpen", &key("b")).unwrap());
    assert!(store.stop("textpen", "ed25519:another").unwrap());
    assert_eq!(
        store.stopped("textpen").unwrap().as_deref(),
        Some("ed25519:another")
    );
    assert!(matches!(
        store.stop("textpen", "two\nlines"),
        Err(Error::Record(_))
    ));
}

#[test]
fn a_release_signed_with_the_approved_key_ends_the_stop() {
    let home = Home::new();
    install(&home, &T100).unwrap();
    home.store().stop("textpen", &key("b")).unwrap();
    install(&home, &T110).unwrap();
    assert_eq!(home.store().stopped("textpen").unwrap(), None);
}

#[test]
fn this_machine_is_a_kind_a_release_is_made_for() {
    let target = kablo_package::this_target().unwrap();
    assert!(
        [
            "aarch64-apple-darwin",
            "x86_64-apple-darwin",
            "x86_64-unknown-linux-musl",
            "aarch64-unknown-linux-musl"
        ]
        .contains(&target)
    );
}

#[test]
fn the_home_has_the_folder_bateri_started() {
    let store = Store::in_home(Path::new("/home/someone"));
    assert_eq!(
        store.root(),
        Path::new("/home/someone/.local/share/bateri/apps")
    );
}
