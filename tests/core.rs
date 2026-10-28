use std::io::Write as _;
use std::process::{Command, Stdio};

use engram::memory::{self, Write};
use engram::{core, db, spaces};
use rusqlite::Connection;
use tempfile::TempDir;

const SPACES: &str = r#"
[spaces]
"work/acme" = { domain = "work" }
"personal/side" = { domain = "personal" }
[paths]
"/code/acme" = "work/acme"
"/home/side" = "personal/side"
"#;

fn setup() -> (TempDir, Connection) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("engram.db");
    db::init(&path).unwrap();
    let conn = db::connect(&path).unwrap();
    std::fs::write(dir.path().join("spaces.toml"), SPACES).unwrap();
    spaces::load(&conn, &dir.path().join("spaces.toml")).unwrap();
    (dir, conn)
}

fn put(conn: &Connection, space: &str, kind: &str, title: &str, valid_until: Option<&str>) -> i64 {
    let vis = vec![space.to_string()];
    let w = Write {
        kind,
        title,
        body: title,
        space,
        source: None,
        valid_until,
        mode: Some("add"),
        vector: None,
        visible: &vis,
        status: "active",
    };
    match memory::write(conn, &w, "test").unwrap() {
        memory::Written::Created(id) => id,
        other => panic!("{other:?}"),
    }
}

#[test]
fn core_shows_own_and_global_but_never_the_other_domain() {
    let (_d, conn) = setup();
    let work_rule = put(
        &conn,
        "work/acme",
        "rule",
        "Check rule 02 before replying to the client",
        None,
    );
    let personal_rule = put(
        &conn,
        "personal/side",
        "rule",
        "Blog posts go out on Sundays",
        None,
    );
    let global_pref = put(&conn, "global", "preference", "Answer briefly", None);
    let state = put(
        &conn,
        "work/acme",
        "state",
        "Release 1.4 is frozen",
        Some("2999-01-01"),
    );
    let expired = put(
        &conn,
        "work/acme",
        "state",
        "Old freeze",
        Some("2000-01-01"),
    );
    let decision = put(
        &conn,
        "work/acme",
        "decision",
        "Postgres 17 from now on",
        None,
    );

    let text = core::build(&conn, "work/acme").unwrap();
    for id in [work_rule, global_pref, state, decision] {
        assert!(
            text.contains(&format!("#{id} ")),
            "missing #{id} in\n{text}"
        );
    }
    assert!(
        !text.contains(&format!("#{expired} ")),
        "expired state shown"
    );
    assert!(
        !text.contains(&format!("#{personal_rule} ")),
        "personal leaked into work"
    );
    // own rules come before global ones
    assert!(text.find(&format!("#{work_rule} ")) < text.find(&format!("#{global_pref} ")));

    let personal = core::build(&conn, "personal/side").unwrap();
    assert!(personal.contains(&format!("#{personal_rule} ")));
    assert!(
        !personal.contains(&format!("#{work_rule} ")),
        "work leaked into personal"
    );
}

#[test]
fn core_stays_under_the_cap() {
    let (_d, conn) = setup();
    for i in 0..200 {
        put(
            &conn,
            "work/acme",
            "rule",
            &format!("Rule {i} {}", "x".repeat(100)),
            None,
        );
        put(
            &conn,
            "work/acme",
            "decision",
            &format!("Decision {i} {}", "y".repeat(100)),
            None,
        );
    }
    assert!(core::build(&conn, "work/acme").unwrap().chars().count() <= core::MAX_CHARS);
}

#[test]
fn session_start_hook_prints_the_folder_core() {
    let (dir, conn) = setup();
    let id = put(
        &conn,
        "work/acme",
        "rule",
        "Check rule 02 before replying",
        None,
    );
    drop(conn);

    let run = |stdin: &str| {
        let mut child = Command::new(env!("CARGO_BIN_EXE_engram"))
            .args(["hook", "session-start"])
            .env("ENGRAM_HOME", dir.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success());
        String::from_utf8(out.stdout).unwrap()
    };
    let work = run(r#"{"cwd":"/code/acme/api","hook_event_name":"SessionStart"}"#);
    assert!(work.contains("space work/acme") && work.contains(&format!("#{id} ")));
    let personal = run(r#"{"cwd":"/home/side"}"#);
    assert!(personal.contains("space personal/side") && !personal.contains(&format!("#{id} ")));
    // garbage input still exits 0
    run("not json");
}
