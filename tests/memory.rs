use engram::memory::{self, Query, Scope, Write, Written};
use engram::{db, spaces};
use rusqlite::Connection;
use tempfile::TempDir;

fn setup() -> (TempDir, Connection) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("engram.db");
    db::init(&path).unwrap();
    let conn = db::connect(&path).unwrap();
    spaces::ensure_builtin(&conn).unwrap();
    for (id, domain) in [("work/acme", "work"), ("personal/side", "personal")] {
        conn.execute(
            "INSERT INTO space (id, domain) VALUES (?1, ?2)",
            [id, domain],
        )
        .unwrap();
    }
    (dir, conn)
}

/// Unit vector along one axis, so similarity is exactly 0 or 1.
fn axis(i: usize) -> Vec<f32> {
    let mut v = vec![0f32; 1024];
    v[i] = 1.0;
    v
}

fn visible(conn: &Connection, space: &str) -> Vec<String> {
    spaces::visible(conn, space).unwrap()
}

fn put(
    conn: &Connection,
    space: &str,
    kind: &str,
    title: &str,
    body: &str,
    v: Option<Vec<f32>>,
) -> i64 {
    let vis = vec![space.to_string()];
    match memory::write(
        conn,
        &Write {
            kind,
            title,
            body,
            space,
            source: None,
            valid_until: None,
            mode: Some("add"),
            vector: v.as_deref(),
            visible: &vis,
            status: "active",
        },
        "test",
    )
    .unwrap()
    {
        Written::Created(id) => id,
        other => panic!("{other:?}"),
    }
}

fn search(
    conn: &Connection,
    text: &str,
    v: Option<Vec<f32>>,
    spaces: &[String],
    scope: Scope,
) -> Vec<i64> {
    memory::search(
        conn,
        &Query {
            text,
            vector: v.as_deref(),
            spaces,
            scope,
            kind: None,
            limit: 8,
            as_of: None,
        },
    )
    .unwrap()
    .iter()
    .map(|h| h.id)
    .collect()
}

#[test]
fn search_respects_spaces_and_finds_by_words_or_vector() {
    let (_d, conn) = setup();
    let work = put(
        &conn,
        "work/acme",
        "gotcha",
        "Deploy needs VPN",
        "The stand is reachable only over VPN",
        Some(axis(1)),
    );
    let personal = put(
        &conn,
        "personal/side",
        "fact",
        "Deploy the blog",
        "Blog deploys from main via VPN-free CI",
        Some(axis(2)),
    );
    let global = put(
        &conn,
        "global",
        "preference",
        "Short answers",
        "Answer briefly, details on request",
        Some(axis(3)),
    );

    let from_work = visible(&conn, "work/acme");
    assert_eq!(
        search(&conn, "deploy", None, &from_work, Scope::Project),
        [work]
    );
    assert_eq!(
        search(&conn, "", Some(axis(3)), &from_work, Scope::Project)[0],
        global
    );
    // short words go through the unicode61 table
    let short = put(&conn, "work/acme", "fact", "CI is on", "Uses GH CI", None);
    assert!(search(&conn, "CI", None, &from_work, Scope::Project).contains(&short));

    let from_personal = visible(&conn, "personal/side");
    let found = search(
        &conn,
        "deploy",
        Some(axis(1)),
        &from_personal,
        Scope::Project,
    );
    assert!(
        found.contains(&personal) && !found.contains(&work),
        "work never leaks into personal"
    );

    // `all` widens to the whole domain but never crosses it
    conn.execute(
        "INSERT INTO space (id, domain) VALUES ('work/other', 'work')",
        [],
    )
    .unwrap();
    let sibling = put(
        &conn,
        "work/other",
        "fact",
        "Deploy on Fridays",
        "Never",
        None,
    );
    let wide = memory::spaces_for(&conn, "work/acme", Scope::All).unwrap();
    let found = search(&conn, "deploy", None, &wide, Scope::All);
    assert!(found.contains(&work) && found.contains(&sibling) && !found.contains(&personal));
    let wide = memory::spaces_for(&conn, "personal/side", Scope::All).unwrap();
    let found = search(&conn, "deploy", Some(axis(1)), &wide, Scope::All);
    assert!(
        found.contains(&personal) && !found.contains(&work) && !found.contains(&sibling),
        "scope=all from personal must not see work"
    );
    assert!(search(&conn, "deploy", None, &[], Scope::All).is_empty());
}

#[test]
fn write_asks_before_near_duplicates_and_supports_modes() {
    let (_d, conn) = setup();
    let vis = visible(&conn, "work/acme");
    let old = put(
        &conn,
        "work/acme",
        "decision",
        "Use Postgres",
        "We use Postgres 16",
        Some(axis(5)),
    );
    let base = Write {
        kind: "decision",
        title: "Use Postgres 17",
        body: "We moved to Postgres 17",
        space: "work/acme",
        source: None,
        valid_until: None,
        mode: None,
        vector: Some(&axis(5)),
        visible: &vis,
        status: "active",
    };

    match memory::write(&conn, &base, "test").unwrap() {
        Written::Similar(s) => assert_eq!(s[0].0.id, old),
        other => panic!("{other:?}"),
    }
    let mode = format!("supersede:{old}");
    let Written::Superseded { new, old: o } = memory::write(
        &conn,
        &Write {
            mode: Some(&mode),
            ..base
        },
        "test",
    )
    .unwrap() else {
        panic!()
    };
    assert_eq!(o, old);
    let (status, by): (String, i64) = conn
        .query_row(
            "SELECT status, superseded_by FROM item WHERE id = ?1",
            [old],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((status.as_str(), by), ("superseded", new));
    assert!(!search(&conn, "postgres", None, &vis, Scope::Project).contains(&old));
    assert!(search(&conn, "postgres", None, &vis, Scope::Archive).contains(&old));

    let mode = format!("update:{new}");
    let upd = Write {
        body: "We moved to Postgres 17.2",
        mode: Some(&mode),
        ..base
    };
    assert!(
        matches!(memory::write(&conn, &upd, "test").unwrap(), Written::Updated(id) if id == new)
    );
    let before_after: i64 = conn
        .query_row(
            "SELECT count(*) FROM log WHERE op = 'update' AND item_id = ?1",
            [new],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(before_after, 1);

    // exact same body is not stored twice, whatever the mode
    let again = Write {
        mode: Some("add"),
        vector: None,
        body: "We moved to Postgres 17.2",
        ..base
    };
    assert!(
        matches!(memory::write(&conn, &again, "test").unwrap(), Written::Exists(id) if id == new)
    );
}

#[test]
fn write_validates_input() {
    let (_d, conn) = setup();
    let vis = visible(&conn, "work/acme");
    let long = "x".repeat(1501);
    let ok = Write {
        kind: "fact",
        title: "t",
        body: "b",
        space: "work/acme",
        source: None,
        valid_until: None,
        mode: Some("add"),
        vector: None,
        visible: &vis,
        status: "active",
    };
    assert!(
        memory::write(
            &conn,
            &Write {
                kind: "checkpoint",
                ..ok
            },
            "t"
        )
        .is_err()
    );
    assert!(memory::write(&conn, &Write { body: &long, ..ok }, "t").is_err());
    assert!(
        memory::write(
            &conn,
            &Write {
                space: "personal/side",
                ..ok
            },
            "t"
        )
        .is_err()
    );
    assert!(
        memory::write(
            &conn,
            &Write {
                mode: Some("merge:1"),
                ..ok
            },
            "t"
        )
        .is_err()
    );
}

#[test]
fn get_counts_hits_and_marks_recall_open() {
    let (_d, conn) = setup();
    let vis = visible(&conn, "work/acme");
    let a = put(
        &conn,
        "work/acme",
        "fact",
        "Port is 7777",
        "UI listens on 7777",
        None,
    );
    let hits = memory::search(
        &conn,
        &Query {
            text: "port",
            vector: None,
            spaces: &vis,
            scope: Scope::Project,
            kind: None,
            limit: 8,
            as_of: None,
        },
    )
    .unwrap();
    memory::log_recall(&conn, "s1", "work/acme", "port", &hits).unwrap();
    let text = memory::get(&conn, &[a, 999], "s1", None).unwrap();
    assert!(text.contains("UI listens on 7777") && text.contains("#999 not found"));
    let (n, opened): (i64, String) = conn
        .query_row(
            "SELECT (SELECT hits FROM item WHERE id = ?1), (SELECT opened FROM recall WHERE session = 's1')",
            [a],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((n, opened), (1, format!("[{a}]")));
}

#[test]
fn sessions_cannot_touch_or_read_other_domains() {
    let (_d, conn) = setup();
    let work = put(
        &conn,
        "work/acme",
        "fact",
        "Stand IP",
        "The stand is at 10.0.0.5",
        None,
    );
    let personal = visible(&conn, "personal/side");
    for mode in [format!("update:{work}"), format!("supersede:{work}")] {
        let w = Write {
            kind: "fact",
            title: "x",
            body: "overwritten",
            space: "personal/side",
            source: None,
            valid_until: None,
            mode: Some(&mode),
            vector: None,
            visible: &personal,
            status: "active",
        };
        assert!(
            memory::write(&conn, &w, "t").is_err(),
            "{mode} crossed domains"
        );
    }
    let body: String = conn
        .query_row("SELECT body FROM item WHERE id = ?1", [work], |r| r.get(0))
        .unwrap();
    assert_eq!(body, "The stand is at 10.0.0.5");
    assert!(
        memory::get(&conn, &[work], "s", Some(&personal))
            .unwrap()
            .contains("not found")
    );
    assert!(
        memory::get(&conn, &[work], "s", None)
            .unwrap()
            .contains("10.0.0.5")
    );

    // the same text in another domain is stored on its own, not reported as "exists"
    let w = Write {
        kind: "fact",
        title: "Stand IP",
        body: "The stand is at 10.0.0.5",
        space: "personal/side",
        source: None,
        valid_until: None,
        mode: Some("add"),
        vector: None,
        visible: &personal,
        status: "active",
    };
    let mine = match memory::write(&conn, &w, "t").unwrap() {
        Written::Created(id) => id,
        other => panic!("cross-domain duplicate leaked: {other:?}"),
    };

    // links to items the session cannot see are hidden
    conn.execute(
        "INSERT INTO edge (src, dst, type, confirmed) VALUES (?1, ?2, 'about', 1)",
        [mine, work],
    )
    .unwrap();
    let got = memory::get(&conn, &[mine], "s", Some(&personal)).unwrap();
    assert!(!got.contains(&format!("#{work}")), "{got}");

    // global items are shared by every session, so no session rewrites or retires them
    let shared = put(&conn, "global", "rule", "Be brief", "Short answers", None);
    for mode in [format!("update:{shared}"), format!("supersede:{shared}")] {
        let w = Write {
            mode: Some(&mode),
            ..w
        };
        assert!(memory::write(&conn, &w, "t").is_err(), "{mode} on global");
    }
}

/// Unit vector with the given cosine to `axis(0)`.
fn at_cos(cos: f32) -> Vec<f32> {
    let mut v = vec![0f32; 1024];
    v[0] = cos;
    v[1] = (1.0 - cos * cos).sqrt();
    v
}

#[test]
fn related_lists_same_topic_records_that_write_would_not_refuse() {
    let (_d, conn) = setup();
    let vis = visible(&conn, "work/acme");
    let write = |space: &str, title: &str, v: Vec<f32>| {
        memory::write(
            &conn,
            &Write {
                kind: "fact",
                title,
                body: title,
                space,
                source: None,
                valid_until: None,
                mode: Some("add"),
                vector: Some(&v),
                visible: &visible(&conn, space),
                status: "active",
            },
            "test",
        )
        .unwrap()
    };
    let Written::Created(topic) = write("work/acme", "Laptop is a 2017 MacBook", at_cos(0.80))
    else {
        panic!()
    };
    write("work/acme", "Too close, write refuses it", at_cos(0.95));
    write("work/acme", "Unrelated", at_cos(0.40));
    write("global", "Global on the same topic", at_cos(0.80));
    write(
        "personal/side",
        "Other domain on the same topic",
        at_cos(0.80),
    );

    let found = memory::related(&conn, &axis(0), &vis).unwrap();
    let mut titles: Vec<&str> = found.iter().map(|(h, _)| h.title.as_str()).collect();
    titles.sort();
    assert_eq!(
        titles,
        ["Global on the same topic", "Laptop is a 2017 MacBook"],
        "visible same-topic records only"
    );
    assert!(found.iter().any(|(h, _)| h.id == topic));
}

#[test]
fn supersede_retires_the_old_record_even_if_the_new_text_is_already_stored() {
    let (_d, conn) = setup();
    let vis = visible(&conn, "work/acme");
    let write = |title: &str, mode: Option<&str>| {
        memory::write(
            &conn,
            &Write {
                kind: "fact",
                title,
                body: title,
                space: "work/acme",
                source: None,
                valid_until: None,
                mode,
                vector: None,
                visible: &vis,
                status: "active",
            },
            "test",
        )
        .unwrap()
    };
    let Written::Created(old) = write("Laptop is a 2017 MacBook", Some("add")) else {
        panic!()
    };
    let Written::Created(new) = write("Laptop is a new MacBook", Some("add")) else {
        panic!()
    };
    let mode = format!("supersede:{old}");
    match write("Laptop is a new MacBook", Some(&mode)) {
        Written::Superseded { new: n, old: o } => assert_eq!((n, o), (new, old)),
        other => panic!("{other:?}"),
    }
    let status: String = conn
        .query_row("SELECT status FROM item WHERE id = ?1", [old], |r| r.get(0))
        .unwrap();
    assert_eq!(status, "superseded");
    // a record cannot retire itself
    let own = format!("supersede:{new}");
    assert!(matches!(
        write("Laptop is a new MacBook", Some(&own)),
        Written::Exists(_)
    ));
}
