use engram::memory::{self, Write, Written};
use engram::{db, review, spaces, tui};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::KeyCode;
use rusqlite::Connection;
use serde_json::json;
use tempfile::TempDir;

fn setup() -> (TempDir, Connection) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("engram.db");
    db::init(&path).unwrap();
    let conn = db::connect(&path).unwrap();
    std::fs::write(
        dir.path().join("spaces.toml"),
        r#"
[spaces]
"work/acme" = { domain = "work" }
"personal/notes" = { domain = "personal" }
"#,
    )
    .unwrap();
    spaces::load(&conn, &dir.path().join("spaces.toml")).unwrap();
    (dir, conn)
}

fn put(conn: &Connection, space: &str, title: &str) -> i64 {
    let visible = vec![space.to_string()];
    match memory::write(
        conn,
        &Write {
            kind: "fact",
            title,
            body: title,
            space,
            source: None,
            valid_until: None,
            mode: Some("add"),
            vector: None,
            visible: &visible,
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

fn screen(app: &mut tui::App) -> String {
    let mut t = Terminal::new(TestBackend::new(140, 40)).unwrap();
    t.draw(|f| app.draw(f)).unwrap();
    let buf = t.backend().buffer();
    (0..40)
        .map(|y| (0..140).map(|x| buf[(x, y)].symbol()).collect::<String>() + "\n")
        .collect()
}

#[test]
fn map_shows_domains_and_spaces_and_opens_a_space() {
    let (_d, conn) = setup();
    put(&conn, "work/acme", "Deploy needs VPN");
    put(&conn, "work/acme", "Stand is on port 8080");
    put(&conn, "personal/notes", "Blog runs on Zola");
    let mut app = tui::App::new(conn).unwrap();

    let map = screen(&mut app);
    for label in [
        "memory map",
        "global",
        "work",
        "personal",
        "acme 2",
        "notes 1",
    ] {
        assert!(map.contains(label), "{label} missing:\n{map}");
    }
    // the biggest space is selected first, and Enter opens its records
    assert!(map.contains("work/acme"));
    app.press(KeyCode::Enter).unwrap();
    let space = screen(&mut app);
    assert!(space.contains("work/acme · 2 records"), "{space}");
    assert!(space.contains("Deploy needs VPN") && !space.contains("Blog runs on Zola"));
}

#[test]
fn queue_puts_global_first_and_accepts_with_y() {
    let (_d, conn) = setup();
    let id = put(&conn, "work/acme", "Prefer short answers");
    review::add(
        &conn,
        "new",
        &[],
        &json!({"kind": "fact", "title": "Release is on Fridays", "body": "Fridays", "space": "work/acme"}),
        "session:x",
        "open",
    )
    .unwrap();
    review::add(
        &conn,
        "move",
        &[id],
        &json!({"space": "global"}),
        "true in every project",
        "open",
    )
    .unwrap();
    let mut app = tui::App::new(conn).unwrap();
    app.press(KeyCode::Char('2')).unwrap();
    let queue = screen(&mut app);
    assert!(queue.contains("2 open · 1 for global"), "{queue}");
    let first = queue.find("global Prefer short answers").unwrap();
    assert!(first < queue.find("Release is on Fridays").unwrap());

    app.press(KeyCode::Char('y')).unwrap();
    let after = screen(&mut app);
    assert!(after.contains("1 open · 0 for global"), "{after}");
}
