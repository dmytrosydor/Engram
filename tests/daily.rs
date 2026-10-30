use std::cell::RefCell;
use std::path::Path;

use anyhow::Result;
use engram::daily::{self, Llm, Options};
use engram::memory::{self, Write, Written};
use engram::{db, review, spaces};
use rusqlite::{Connection, params};
use tempfile::TempDir;

const SPACES: &str = r#"
[spaces]
"work/acme" = { domain = "work" }
[paths]
"/code/acme" = "work/acme"
"#;

/// Answers each prompt type with a canned reply and records what it was asked.
struct Fake {
    asked: RefCell<Vec<String>>,
    classify: String,
}

impl Llm for Fake {
    fn ask(&self, prompt: &str) -> Result<String> {
        self.asked.borrow_mut().push(prompt.to_string());
        Ok(if prompt.contains("PAIR a=") {
            let a = between(prompt, "PAIR a=", " ");
            let b = between(prompt, " b=", " ");
            format!(r#"Sure! [{{"a": {a}, "b": {b}, "verdict": "same", "keep": {a}}}]"#)
        } else if prompt.contains("imported from an old memory store") {
            self.classify.clone()
        } else {
            r#"```json
[{"kind": "gotcha", "title": "Stand needs VPN", "body": "The acme stand only answers over VPN.", "scope": "project"},
 {"kind": "nonsense", "title": "x", "body": "y"}]
```"#
                .to_string()
        })
    }
}

fn between<'a>(s: &'a str, start: &str, end: &str) -> &'a str {
    let i = s.find(start).unwrap() + start.len();
    let j = s[i..].find(end).unwrap() + i;
    &s[i..j]
}

fn axis(i: usize) -> Vec<f32> {
    let mut v = vec![0f32; 1024];
    v[i] = 1.0;
    v
}

fn put(
    conn: &Connection,
    kind: &str,
    title: &str,
    valid_until: Option<&str>,
    v: Option<Vec<f32>>,
) -> i64 {
    let vis = vec!["work/acme".to_string()];
    let w = Write {
        kind,
        title,
        body: title,
        space: "work/acme",
        source: None,
        valid_until,
        mode: Some("add"),
        vector: v.as_deref(),
        visible: &vis,
        status: "active",
    };
    match memory::write(conn, &w, "test").unwrap() {
        Written::Created(id) => id,
        other => panic!("{other:?}"),
    }
}

fn status(conn: &Connection, id: i64) -> (String, String, String) {
    conn.query_row(
        "SELECT status, kind, space_id FROM item WHERE id = ?1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .unwrap()
}

fn transcript(dir: &Path) {
    let proj = dir.join("projects/-code-acme");
    std::fs::create_dir_all(&proj).unwrap();
    let long = "We found that the stand only answers over VPN. ".repeat(20);
    let lines = [
        serde_json::json!({"type": "user", "cwd": "/code/acme", "message": {"role": "user", "content": "why is the stand down?"}}),
        serde_json::json!({"type": "assistant", "cwd": "/code/acme", "message": {"role": "assistant", "content": [
            {"type": "thinking", "thinking": "secret"}, {"type": "text", "text": long}]}}),
        serde_json::json!({"type": "user", "isSidechain": true, "message": {"content": "subagent noise"}}),
    ];
    let body: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
    std::fs::write(proj.join("s1.jsonl"), body.join("\n")).unwrap();
}

#[test]
fn daily_pass_end_to_end_with_a_fake_model() {
    // no embedding model in tests: fail fast, extraction then skips the dedupe check
    unsafe {
        std::env::set_var("ENGRAM_EMBED_SOCK", "/nonexistent/engram-test.sock");
        std::env::set_var("ENGRAM_LLAMA_SERVER", "/usr/bin/false");
    }
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("engram.db");
    db::init(&path).unwrap();
    let conn = db::connect(&path).unwrap();
    std::fs::write(dir.path().join("spaces.toml"), SPACES).unwrap();
    spaces::load(&conn, &dir.path().join("spaces.toml")).unwrap();

    let expired = put(
        &conn,
        "state",
        "Freeze until Friday",
        Some("2000-01-01"),
        None,
    );
    let a = put(&conn, "decision", "Use Postgres 17", None, Some(axis(7)));
    let b = put(
        &conn,
        "decision",
        "We use Postgres 17 now",
        None,
        Some(axis(7)),
    );
    let p1 = put(&conn, "fact", "Old progress note", None, None);
    let p2 = put(
        &conn,
        "fact",
        "Agents should re-run failing tests once",
        None,
        None,
    );
    conn.execute(
        "UPDATE item SET status = 'proposed' WHERE id IN (?1, ?2)",
        params![p1, p2],
    )
    .unwrap();
    transcript(dir.path());
    std::fs::create_dir_all(dir.path().join("inbox")).unwrap();
    std::fs::write(
        dir.path().join("inbox/idea.md"),
        "Remember: the acme API rate limit is 50 rps.",
    )
    .unwrap();

    let fake = Fake {
        asked: RefCell::new(Vec::new()),
        classify: format!(
            r#"[{{"id": {p1}, "verdict": "archive", "kind": "event", "scope": "project"}},
                {{"id": {p2}, "verdict": "keep", "kind": "lesson", "scope": "global"}},
                {{"id": 999999, "verdict": "keep"}}]"#
        ),
    };
    let opts = Options {
        classify: 100,
        transcripts: 5,
        pairs: 10,
        projects: dir.path().join("projects"),
        inbox_dir: dir.path().join("inbox"),
        exclude: vec![],
    };
    let r = daily::run(&conn, &fake, &opts, |_| {}).unwrap();
    assert!(r.errors.is_empty(), "{:?}", r.errors);

    assert_eq!(status(&conn, expired).0, "archived");
    assert_eq!(
        status(&conn, p1),
        ("archived".into(), "event".into(), "work/acme".into())
    );
    // the move to global waits for a person
    assert_eq!(
        status(&conn, p2),
        ("active".into(), "lesson".into(), "work/acme".into())
    );
    assert_eq!((r.pairs_queued, r.transcripts, r.notes), (1, 1, 1));
    // one valid item per extraction (the invalid kind is dropped): transcript + note
    assert_eq!(r.proposals, 2);

    let transcript_prompt = fake
        .asked
        .borrow()
        .iter()
        .find(|p| p.contains("VPN"))
        .cloned()
        .unwrap();
    assert!(!transcript_prompt.contains("secret") && !transcript_prompt.contains("subagent noise"));
    assert!(dir.path().join("inbox/processed/idea.md").exists());

    // accepting the merge supersedes the duplicate; the new proposal becomes an item
    let open = review::list(&conn, "open", 10).unwrap();
    let mv = open.iter().find(|e| e.op == "move").unwrap();
    review::accept(&conn, mv.id, None, "test").unwrap();
    assert_eq!(status(&conn, p2).2, "global");
    let merge = open.iter().find(|e| e.op == "merge").unwrap();
    review::accept(&conn, merge.id, None, "test").unwrap();
    assert_eq!(status(&conn, b).0, "superseded");
    assert_eq!(status(&conn, a).0, "active");
    let new = open.iter().find(|e| e.op == "new").unwrap();
    review::accept(&conn, new.id, None, "test").unwrap();
    let saved: i64 = conn
        .query_row(
            "SELECT count(*) FROM item WHERE title = 'Stand needs VPN' AND status = 'active'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(saved, 1);
    assert!(
        review::accept(&conn, new.id, None, "test").is_err(),
        "cannot accept twice"
    );

    // second run the same day: nothing left to do, pair not asked again
    let asked_before = fake.asked.borrow().len();
    let again = daily::run(&conn, &fake, &opts, |_| {}).unwrap();
    assert_eq!(
        (again.pairs_found, again.classified, again.transcripts),
        (0, 0, 0)
    );
    assert_eq!(fake.asked.borrow().len(), asked_before);
    assert!(daily::not_due(&conn, true).unwrap().is_some());
}

#[test]
fn bad_transcripts_are_skipped_and_the_rest_wait_for_the_next_run() {
    unsafe {
        std::env::set_var("ENGRAM_EMBED_SOCK", "/nonexistent/engram-test.sock");
        std::env::set_var("ENGRAM_LLAMA_SERVER", "/usr/bin/false");
    }
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("engram.db");
    db::init(&path).unwrap();
    let conn = db::connect(&path).unwrap();
    std::fs::write(dir.path().join("spaces.toml"), SPACES).unwrap();
    spaces::load(&conn, &dir.path().join("spaces.toml")).unwrap();

    let proj = dir.path().join("projects/-code-acme");
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::write(proj.join("0-broken.jsonl"), [0xff, 0xfe, b'\n', 0xff]).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    transcript(dir.path()); // s1.jsonl, newer
    std::thread::sleep(std::time::Duration::from_millis(20));
    let s2 = std::fs::read_to_string(proj.join("s1.jsonl"))
        .unwrap()
        .replace("stand", "box");
    std::fs::write(proj.join("s2.jsonl"), s2).unwrap();
    std::fs::create_dir_all(dir.path().join("inbox")).unwrap();
    std::fs::write(dir.path().join("inbox/bin.md"), [0xff, b'x']).unwrap();

    let fake = Fake {
        asked: RefCell::new(Vec::new()),
        classify: "[]".into(),
    };
    let opts = Options {
        classify: 0,
        transcripts: 2,
        pairs: 0,
        projects: dir.path().join("projects"),
        inbox_dir: dir.path().join("inbox"),
        exclude: vec![],
    };
    let r = daily::run(&conn, &fake, &opts, |_| {}).unwrap();
    // the broken file is reported, s1 is read, s2 is left for the next run
    assert_eq!(r.errors.len(), 1, "{:?}", r.errors);
    assert_eq!(
        (r.transcripts, r.notes),
        (1, 1),
        "non-UTF-8 note is stored lossily"
    );
    assert!(dir.path().join("inbox/processed/bin.md").exists());

    conn.execute("DELETE FROM meta WHERE key = 'daily_last_run'", [])
        .unwrap();
    let next = daily::run(&conn, &fake, &opts, |_| {}).unwrap();
    assert_eq!(
        (next.transcripts, next.errors.len()),
        (1, 0),
        "s2 picked up, nothing repeated"
    );
}

#[test]
fn default_pass_archives_expired_state_and_runs_once_a_day_without_a_model() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("engram.db");
    db::init(&path).unwrap();
    let conn = db::connect(&path).unwrap();
    std::fs::write(dir.path().join("spaces.toml"), SPACES).unwrap();
    spaces::load(&conn, &dir.path().join("spaces.toml")).unwrap();

    let old = put(&conn, "state", "Old checkpoint", Some("2020-01-01"), None);
    let open = put(&conn, "state", "Open checkpoint", Some("2999-01-01"), None);
    let fact = put(&conn, "fact", "Stand is on port 8080", None, None);

    assert!(daily::not_due(&conn, false).unwrap().is_none());
    let r = daily::tidy(&conn, |_| {}).unwrap();
    assert_eq!(r.expired, 1);
    assert!(r.cores > 0);
    assert_eq!(status(&conn, old).0, "archived");
    assert_eq!(status(&conn, open).0, "active");
    assert_eq!(status(&conn, fact).0, "active");

    assert!(daily::not_due(&conn, false).unwrap().is_some());
    let queue: i64 = conn
        .query_row("SELECT count(*) FROM review", [], |r| r.get(0))
        .unwrap();
    assert_eq!(queue, 0, "the default pass queues nothing");
}
