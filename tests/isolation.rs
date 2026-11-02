//! Regression tests from an isolation audit: hidden global proposals, spaces.toml
//! guards, domain-mixing in the daily pass, and concurrent writers.

use std::cell::RefCell;

use anyhow::Result;
use engram::daily::{self, Llm, Options};
use engram::memory::{self, Query, Scope, Write, Written};
use engram::{db, review, spaces, store};
use rusqlite::Connection;
use serde_json::json;
use tempfile::TempDir;

fn setup() -> (TempDir, Connection) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("engram.db");
    db::init(&path).unwrap();
    let conn = db::connect(&path).unwrap();
    spaces::ensure_builtin(&conn).unwrap();
    for (id, domain) in [
        ("work/acme", "work"),
        ("work/_shared", "work"),
        ("personal/side", "personal"),
        ("personal/_shared", "personal"),
    ] {
        conn.execute(
            "INSERT INTO space (id, domain) VALUES (?1, ?2)",
            [id, domain],
        )
        .unwrap();
    }
    (dir, conn)
}

fn axis(i: usize) -> Vec<f32> {
    let mut v = vec![0f32; 1024];
    v[i] = 1.0;
    v
}

fn vis(conn: &Connection, s: &str) -> Vec<String> {
    spaces::visible(conn, s).unwrap()
}

fn w<'a>(
    space: &'a str,
    title: &'a str,
    body: &'a str,
    visible: &'a [String],
    status: &'a str,
    mode: Option<&'a str>,
) -> Write<'a> {
    Write {
        kind: "fact",
        title,
        body,
        space,
        source: None,
        valid_until: None,
        mode,
        vector: None,
        visible,
        status,
    }
}

fn status_of(conn: &Connection, id: i64) -> String {
    conn.query_row("SELECT status FROM item WHERE id = ?1", [id], |r| r.get(0))
        .unwrap()
}

/// Writing text another domain has proposed to global must not reveal that it exists.
#[test]
fn hidden_global_proposal_does_not_leak_through_duplicates() {
    let (_d, conn) = setup();
    let work = vis(&conn, "work/acme");
    let personal = vis(&conn, "personal/side");
    let secret = "Acme layoffs planned for March";
    let Written::Created(hidden) = memory::write(
        &conn,
        &w("global", "secret", secret, &work, "proposed", Some("add")),
        "work",
    )
    .unwrap() else {
        panic!()
    };
    assert!(
        memory::get(&conn, &[hidden], "s", Some(&personal))
            .unwrap()
            .contains("not found")
    );
    let r = memory::write(
        &conn,
        &w(
            "global",
            "probe",
            secret,
            &personal,
            "proposed",
            Some("add"),
        ),
        "personal",
    )
    .unwrap();
    assert!(
        matches!(r, Written::Created(id) if id != hidden),
        "{}",
        r.message()
    );
    // within one domain a repeat is still caught
    let again = memory::write(
        &conn,
        &w("global", "again", secret, &work, "proposed", Some("add")),
        "work",
    )
    .unwrap();
    assert!(matches!(again, Written::Exists(id) if id == hidden));
}

/// supersede:<id> with space=global would retire the session's record now for a
/// replacement that stays hidden and may be rejected; it is refused.
#[test]
fn global_write_cannot_supersede() {
    let (_d, conn) = setup();
    let work = vis(&conn, "work/acme");
    let Written::Created(mine) = memory::write(
        &conn,
        &w(
            "work/acme",
            "mine",
            "Deploy with make",
            &work,
            "active",
            Some("add"),
        ),
        "s",
    )
    .unwrap() else {
        panic!()
    };
    let mode = format!("supersede:{mine}");
    let r = memory::write(
        &conn,
        &w(
            "global",
            "g",
            "Deploy with just",
            &work,
            "proposed",
            Some(&mode),
        ),
        "s",
    );
    assert!(r.is_err(), "{r:?}");
    assert_eq!(status_of(&conn, mine), "active");
    let hits = memory::search(
        &conn,
        &Query {
            text: "deploy make",
            vector: None,
            spaces: &work,
            scope: Scope::Project,
            kind: None,
            limit: 8,
            as_of: None,
        },
    )
    .unwrap();
    assert!(hits.iter().any(|h| h.id == mine));
}

/// A session's global write of an archived global record's text neither rewrites
/// that record nor lets a rejection delete it; accepting it brings the record back.
#[test]
fn session_cannot_rewrite_retired_global_record() {
    let (_d, conn) = setup();
    let all = vec!["global".to_string()];
    let Written::Created(g) = memory::write(
        &conn,
        &Write {
            kind: "rule",
            ..w(
                "global",
                "Old global rule",
                "Use tabs",
                &all,
                "active",
                Some("add"),
            )
        },
        "cli",
    )
    .unwrap() else {
        panic!()
    };
    store::set_status(&conn, g, "archived", None, "cli").unwrap();
    let personal = vis(&conn, "personal/side");

    let Written::Created(p) = memory::write(
        &conn,
        &w(
            "global",
            "HIJACKED",
            "Use tabs",
            &personal,
            "proposed",
            Some("add"),
        ),
        "p",
    )
    .unwrap() else {
        panic!()
    };
    assert_ne!(p, g);
    let title: String = conn
        .query_row("SELECT title FROM item WHERE id = ?1", [g], |r| r.get(0))
        .unwrap();
    assert_eq!(title, "Old global rule");
    assert_eq!(status_of(&conn, g), "archived");

    let rid = review::add(&conn, "activate", &[p], &json!({}), "global write", "open").unwrap();
    review::reject(&conn, rid).unwrap();
    assert_eq!(status_of(&conn, g), "archived", "the old record survives");

    // the same write accepted: the archived record comes back, the proposal folds in
    let Written::Created(p) = memory::write(
        &conn,
        &w(
            "global",
            "again",
            "Use tabs",
            &personal,
            "proposed",
            Some("add"),
        ),
        "p",
    )
    .unwrap() else {
        panic!()
    };
    let rid = review::add(&conn, "activate", &[p], &json!({}), "global write", "open").unwrap();
    let msg = review::accept(&conn, rid, None, "cli").unwrap();
    assert!(msg.contains(&format!("#{g}")), "{msg}");
    assert_eq!(status_of(&conn, g), "active");
    let left: i64 = conn
        .query_row("SELECT count(*) FROM item WHERE id = ?1", [p], |r| r.get(0))
        .unwrap();
    assert_eq!(left, 0);
}

/// An accepted proposal is rehashed under global, so later global duplicates are caught.
#[test]
fn accepted_global_proposal_joins_global_hash_scope() {
    let (_d, conn) = setup();
    let work = vis(&conn, "work/acme");
    let personal = vis(&conn, "personal/side");
    let Written::Created(p) = memory::write(
        &conn,
        &w(
            "global",
            "t",
            "Prefer UTC everywhere",
            &work,
            "proposed",
            Some("add"),
        ),
        "s",
    )
    .unwrap() else {
        panic!()
    };
    let rid = review::add(&conn, "activate", &[p], &json!({}), "global write", "open").unwrap();
    assert_eq!(
        review::accept(&conn, rid, None, "cli").unwrap(),
        format!("#{p} is active")
    );
    let cli = vec!["global".to_string()];
    let r = memory::write(
        &conn,
        &w(
            "global",
            "t",
            "Prefer UTC everywhere",
            &cli,
            "active",
            Some("add"),
        ),
        "cli",
    )
    .unwrap();
    assert!(matches!(r, Written::Exists(id) if id == p), "{r:?}");
    // a new proposal of the now-active text folds into it on accept
    let Written::Created(q) = memory::write(
        &conn,
        &w(
            "global",
            "t",
            "Prefer UTC everywhere",
            &personal,
            "proposed",
            Some("add"),
        ),
        "s",
    )
    .unwrap() else {
        panic!()
    };
    let rid = review::add(&conn, "activate", &[q], &json!({}), "global write", "open").unwrap();
    review::accept(&conn, rid, None, "cli").unwrap();
    let n: i64 = conn
        .query_row(
            "SELECT count(*) FROM item WHERE body = 'Prefer UTC everywhere'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 1);
}

struct Rec(RefCell<Vec<String>>);
impl Llm for Rec {
    fn ask(&self, prompt: &str) -> Result<String> {
        self.0.borrow_mut().push(prompt.to_string());
        Ok("[]".into())
    }
}

/// Near-duplicate pairs from two domains never share a prompt.
#[test]
fn daily_pairs_are_batched_per_domain() {
    unsafe {
        std::env::set_var("ENGRAM_EMBED_SOCK", "/nonexistent/engram-test.sock");
        std::env::set_var("ENGRAM_LLAMA_SERVER", "/usr/bin/false");
    }
    let (d, conn) = setup();
    let put = |space: &str, title: &str, v: usize| {
        let vis = vec![space.to_string()];
        let Written::Created(id) = memory::write(
            &conn,
            &Write {
                vector: Some(&axis(v)),
                ..w(space, title, title, &vis, "active", Some("add"))
            },
            "t",
        )
        .unwrap() else {
            panic!()
        };
        id
    };
    put("work/acme", "WORKSECRET one", 3);
    put("work/acme", "WORKSECRET two", 3);
    put("personal/side", "PERSONALSECRET one", 5);
    put("personal/side", "PERSONALSECRET two", 5);
    let llm = Rec(RefCell::new(vec![]));
    let opts = Options {
        classify: 0,
        transcripts: 0,
        pairs: 40,
        projects: d.path().join("p"),
        inbox_dir: d.path().join("i"),
        exclude: vec![],
    };
    daily::run(&conn, &llm, &opts, |_| {}).unwrap();
    let prompts = llm.0.borrow();
    assert!(prompts.iter().any(|p| p.contains("WORKSECRET")));
    assert!(prompts.iter().any(|p| p.contains("PERSONALSECRET")));
    for p in prompts.iter() {
        assert!(
            !(p.contains("WORKSECRET") && p.contains("PERSONALSECRET")),
            "one prompt carries two domains:\n{p}"
        );
    }
}

#[test]
fn spaces_toml_cannot_move_builtin_spaces() {
    let (d, conn) = setup();
    let f = d.path().join("spaces.toml");
    for body in [
        "[spaces]\n\"unsorted\" = { domain = \"work\" }\n",
        "[spaces]\n\"global\" = { domain = \"work\" }\n",
    ] {
        std::fs::write(&f, body).unwrap();
        assert!(spaces::load(&conn, &f).is_err(), "{body}");
    }
    assert_eq!(
        spaces::reachable(&conn, "unsorted").unwrap(),
        ["global", "unsorted"]
    );
    // listing a built-in space with its own domain stays allowed
    std::fs::write(&f, "[spaces]\n\"global\" = { domain = \"global\" }\n").unwrap();
    spaces::load(&conn, &f).unwrap();
}

#[test]
fn spaces_toml_cannot_give_shared_space_to_another_domain() {
    let (d, conn) = setup();
    let f = d.path().join("spaces.toml");
    std::fs::write(
        &f,
        "[spaces]\n\"personal/_shared\" = { domain = \"work\" }\n",
    )
    .unwrap();
    assert!(spaces::load(&conn, &f).is_err());
    assert_eq!(
        spaces::domain(&conn, "personal/_shared").unwrap(),
        "personal"
    );
}

/// A folder mapped to `global` must not get other domains' unconfirmed global rules.
#[test]
fn core_of_global_space_skips_hidden_proposals() {
    let (_d, conn) = setup();
    let work = vis(&conn, "work/acme");
    memory::write(
        &conn,
        &Write {
            kind: "rule",
            ..w(
                "global",
                "WORKSECRET rule",
                "x",
                &work,
                "proposed",
                Some("add"),
            )
        },
        "s",
    )
    .unwrap();
    let core = engram::core::build(&conn, "global").unwrap();
    assert!(!core.contains("WORKSECRET"), "{core}");
}

/// update:<id> on a hidden global proposal answers exactly like a missing id.
#[test]
fn error_text_does_not_reveal_hidden_global_id() {
    let (_d, conn) = setup();
    let work = vis(&conn, "work/acme");
    let personal = vis(&conn, "personal/side");
    let Written::Created(h) = memory::write(
        &conn,
        &w("global", "s", "hidden", &work, "proposed", Some("add")),
        "s",
    )
    .unwrap() else {
        panic!()
    };
    let m1 = format!("update:{h}");
    let e1 = memory::write(
        &conn,
        &w("personal/side", "x", "y", &personal, "active", Some(&m1)),
        "p",
    )
    .unwrap_err()
    .to_string();
    let e2 = memory::write(
        &conn,
        &w(
            "personal/side",
            "x",
            "y",
            &personal,
            "active",
            Some("update:999"),
        ),
        "p",
    )
    .unwrap_err()
    .to_string();
    assert_eq!(e1.replace(&h.to_string(), "N"), e2.replace("999", "N"));
}

#[test]
fn valid_until_must_be_a_date() {
    let (_d, conn) = setup();
    let work = vis(&conn, "work/acme");
    for bad in ["2026-1-5", "never", "2026-02-30", "tomorrow"] {
        let r = memory::write(
            &conn,
            &Write {
                kind: "state",
                valid_until: Some(bad),
                ..w("work/acme", "s", bad, &work, "active", Some("add"))
            },
            "t",
        );
        assert!(r.is_err(), "{bad} accepted");
    }
    memory::write(
        &conn,
        &Write {
            kind: "state",
            valid_until: Some("2026-12-31"),
            ..w("work/acme", "s", "ok", &work, "active", Some("add"))
        },
        "t",
    )
    .unwrap();
}

/// Parallel tool calls and several sessions write at once. A deferred transaction that
/// reads before it writes fails at once with SQLITE_BUSY under WAL instead of waiting.
#[test]
fn concurrent_writers_all_succeed() {
    let (d, conn) = setup();
    drop(conn);
    let path = d.path().join("engram.db");
    let threads: Vec<_> = (0..8)
        .map(|t| {
            let path = path.clone();
            std::thread::spawn(move || {
                let conn = db::connect(&path).unwrap();
                let visible = spaces::visible(&conn, "work/acme").unwrap();
                let mut errors = Vec::new();
                for i in 0..25 {
                    let body = format!("fact {t}-{i}");
                    if let Err(e) = memory::write(
                        &conn,
                        &w("work/acme", &body, &body, &visible, "active", Some("add")),
                        "t",
                    ) {
                        errors.push(format!("{e:#}"));
                    }
                }
                errors
            })
        })
        .collect();
    let errors: Vec<String> = threads
        .into_iter()
        .flat_map(|t| t.join().unwrap())
        .collect();
    assert!(
        errors.is_empty(),
        "{} failed: {:?}",
        errors.len(),
        errors.first()
    );
    let conn = db::connect(&path).unwrap();
    let n: i64 = conn
        .query_row("SELECT count(*) FROM item", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 200);
}

/// Regression guard: query text that could break an FTS5 MATCH expression.
#[test]
fn odd_queries_do_not_error() {
    let (_d, conn) = setup();
    let work = vis(&conn, "work/acme");
    memory::write(
        &conn,
        &w(
            "work/acme",
            "Hello world",
            "body with $VAR and foo.rs",
            &work,
            "active",
            Some("add"),
        ),
        "t",
    )
    .unwrap();
    let long = "ї".repeat(20000);
    for q in [
        "",
        "\"",
        "$$$",
        "NEAR(a b)",
        "title:x",
        "a AND b OR NOT c",
        "x*",
        "^x",
        "a\0b",
        "{x y}",
        "'; DROP TABLE item;--",
        long.as_str(),
        "🙂🙂🙂",
    ] {
        let r = memory::search(
            &conn,
            &Query {
                text: q,
                vector: None,
                spaces: &work,
                scope: Scope::Project,
                kind: None,
                limit: 8,
                as_of: None,
            },
        );
        assert!(r.is_ok(), "{:?}", r.err());
    }
}
