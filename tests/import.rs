use std::fs;
use std::path::Path;

use engram::{db, import, spaces};
use rusqlite::{Connection, params};
use tempfile::TempDir;

const SPACES: &str = r#"
[spaces]
"work/acme" = { domain = "work" }
"work/acme-api" = { domain = "work" }
"personal/side" = { domain = "personal" }

[paths]
"/code/acme" = "work/acme"
"/code/acme/api" = "work/acme-api"
"/home/side" = "personal/side"

[import]
unknown = "global"
unknown_tasks = "work/acme"
aliases = { "work/old" = "work/acme" }
"#;

fn setup() -> (TempDir, Connection) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("engram.db");
    db::init(&path).unwrap();
    let conn = db::connect(&path).unwrap();
    fs::write(dir.path().join("spaces.toml"), SPACES).unwrap();
    spaces::load(&conn, &dir.path().join("spaces.toml")).unwrap();
    (dir, conn)
}

fn fake_memora(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("memories.db");
    let src = Connection::open(&path).unwrap();
    src.execute_batch(
        "CREATE TABLE memories (id INTEGER PRIMARY KEY, content TEXT NOT NULL, metadata TEXT, tags TEXT,
                                created_at TEXT NOT NULL, updated_at TEXT);
         CREATE TABLE memories_crossrefs (memory_id INTEGER PRIMARY KEY, related TEXT);
         CREATE TABLE tombstones (content_hash TEXT, memory_id INTEGER, reason TEXT, created_at TEXT);",
    )
    .unwrap();
    let rows: [(i64, &str, &str, &str); 7] = [
        (1, "# Use rule 02\nAlways check rule 02.", "{}", "[]"),
        (2, "Old decision", "{}", "[]"),
        (3, "New decision replaces old", "{}", "[]"),
        (4, "Checkpoint 10-12", "{}", "[]"),
        (5, "Part two", r#"{"document_key":"k","ordinal":2}"#, "[]"),
        (6, "Part one", r#"{"document_key":"k","ordinal":1}"#, "[]"),
        (7, "Not in the manifest", "{}", "[]"),
    ];
    for (id, content, meta, tags) in rows {
        src.execute(
            "INSERT INTO memories VALUES (?1, ?2, ?3, ?4, '2026-08-01 10:00:00', NULL)",
            params![id, content, meta, tags],
        )
        .unwrap();
    }
    src.execute(
        "INSERT INTO memories_crossrefs VALUES (2, ?1)",
        [r#"[{"id":3,"score":1.0,"edge_type":"superseded_by"},{"id":1,"score":0.4,"edge_type":"related_to"}]"#],
    )
    .unwrap();
    src.execute(
        "INSERT INTO tombstones VALUES ('abc', 99, 'dup', '2026-08-02')",
        [],
    )
    .unwrap();
    path
}

const MANIFEST: &str = "id,created,space,kind,action,chars,tags,first_line
1,2026-08-01,work/old,feedback,migrate-active,10,,x
2,2026-08-01,work/acme,decision,migrate-active,10,,x
3,2026-08-01,work/acme,decision,migrate-active,10,,x
4,2026-08-01,work/acme,checkpoint,archive-episodic,10,,x
5,2026-08-01,work/acme,document_fragment,documents,10,,x
6,2026-08-01,work/acme,document_fragment,documents,10,,x
";

#[test]
fn memora_import_is_complete_and_idempotent() {
    let (dir, conn) = setup();
    let src = fake_memora(dir.path());
    fs::write(dir.path().join("m.csv"), MANIFEST).unwrap();
    let before = fs::read(&src).unwrap();

    let r = import::memora(&conn, &src, Some(&dir.path().join("m.csv")), None).unwrap();
    assert_eq!(
        (r.items, r.docs, r.chunks, r.edges, r.unmapped),
        (5, 1, 2, 1, 1)
    );

    let row = |sql: &str| -> (String, String, String) {
        conn.query_row(sql, [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
    };
    assert_eq!(
        row(
            "SELECT kind, status, title || ' @ ' || space_id FROM item WHERE legacy_id = 'memora:1'"
        ),
        (
            "rule".into(),
            "active".into(),
            "Use rule 02 @ work/acme".into()
        )
    );
    assert_eq!(
        row("SELECT kind, status, space_id FROM item WHERE legacy_id = 'memora:4'"),
        ("event".into(), "archived".into(), "work/acme".into())
    );
    assert_eq!(
        row("SELECT kind, status, space_id FROM item WHERE legacy_id = 'memora:7'"),
        ("fact".into(), "proposed".into(), spaces::INBOX.into())
    );
    // superseded_by becomes an edge new -> old and retires the old item
    let (old_status, by): (String, i64) = conn
        .query_row(
            "SELECT status, superseded_by FROM item WHERE legacy_id = 'memora:2'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    let new_id: i64 = conn
        .query_row(
            "SELECT id FROM item WHERE legacy_id = 'memora:3'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!((old_status.as_str(), by), ("superseded", new_id));
    // fragments are ordered by their ordinal
    let texts: Vec<String> = conn
        .prepare("SELECT text FROM chunk ORDER BY ord")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(texts, ["Part one", "Part two"]);

    let again = import::memora(&conn, &src, Some(&dir.path().join("m.csv")), None).unwrap();
    assert_eq!((again.items, again.docs), (0, 0));
    let tombstones: i64 = conn
        .query_row("SELECT count(*) FROM log WHERE op = 'tombstone'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(tombstones, 1);
    assert_eq!(fs::read(&src).unwrap(), before, "source untouched");
}

#[test]
fn auto_memory_import() {
    let (dir, conn) = setup();
    let mem = dir.path().join("projects/-code-acme/memory");
    fs::create_dir_all(&mem).unwrap();
    fs::write(mem.join("MEMORY.md"), "- index").unwrap();
    fs::write(
        mem.join("short-answers.md"),
        "---\nname: short-answers\ndescription: Answer briefly\nmetadata:\n  type: feedback\n---\n\nKeep answers short.\n",
    )
    .unwrap();
    fs::write(
        mem.join("me.md"),
        "---\nname: me\ntype: user\n---\nLikes Rust.\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("inv.csv"),
        "claude_project_dir,file,type,chars,proposed_space,action
-code-acme,MEMORY.md,index,7,work/acme,drop (regenerated as space core)
-code-acme,short-answers.md,feedback,50,work/acme,migrate
-code-acme,me.md,user,20,work/acme,global
-code-acme,gone.md,project,20,work/acme,migrate
",
    )
    .unwrap();

    let r = import::auto_memory(
        &conn,
        &dir.path().join("projects"),
        &dir.path().join("inv.csv"),
    )
    .unwrap();
    assert_eq!((r.items, r.dropped), (2, 2));
    let (title, kind, space): (String, String, String) = conn
        .query_row(
            "SELECT title, kind, space_id FROM item WHERE legacy_id = 'auto:-code-acme/short-answers.md'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        (title.as_str(), kind.as_str(), space.as_str()),
        ("Answer briefly", "rule", "work/acme")
    );
    let pref: String = conn
        .query_row(
            "SELECT space_id FROM item WHERE kind = 'preference'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(pref, "global");
}

#[test]
fn spaces_resolve_and_visibility() {
    let (_dir, conn) = setup();
    let at = |p: &str| spaces::resolve(&conn, Path::new(p)).unwrap();
    assert_eq!(at("/code/acme"), "work/acme");
    assert_eq!(at("/code/acme/web"), "work/acme");
    assert_eq!(at("/code/acme/api/src"), "work/acme-api"); // longest prefix wins
    assert_eq!(at("/code/acme-other"), spaces::INBOX); // not a path prefix match
    assert_eq!(
        spaces::visible(&conn, "work/acme").unwrap(),
        ["work/acme", "work/_shared", "global"]
    );
    assert_eq!(spaces::visible(&conn, "global").unwrap(), ["global"]);
    // a folder without a rule sees no domain at all
    assert_eq!(
        spaces::visible(&conn, spaces::INBOX).unwrap(),
        [spaces::INBOX, "global"]
    );
}

#[test]
fn auto_memory_scan_places_projects_by_folder_rules() {
    let (dir, conn) = setup();
    let projects = dir.path().join("projects");
    for (project, file, text) in [
        (
            "-code-acme",
            "deploy.md",
            "---\ntype: feedback\n---\nDeploy only on weekdays.\n",
        ),
        ("-code-acme-api", "limits.md", "API rate limit is 50 rps.\n"),
        ("-tmp-scratch", "note.md", "Scratch folder note.\n"),
        ("-code-acme", "MEMORY.md", "- index\n"),
    ] {
        let mem = projects.join(project).join("memory");
        fs::create_dir_all(&mem).unwrap();
        fs::write(mem.join(file), text).unwrap();
    }
    let r = import::auto_memory_scan(&conn, &projects).unwrap();
    assert_eq!((r.items, r.dropped, r.unmapped), (3, 1, 1));
    let placed = |legacy: &str| -> (String, String, String) {
        conn.query_row(
            "SELECT space_id, kind, status FROM item WHERE legacy_id = ?1",
            [legacy],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap()
    };
    assert_eq!(
        placed("auto:-code-acme/deploy.md"),
        ("work/acme".into(), "rule".into(), "active".into())
    );
    assert_eq!(placed("auto:-code-acme-api/limits.md").0, "work/acme-api");
    assert_eq!(
        placed("auto:-tmp-scratch/note.md"),
        (spaces::INBOX.into(), "fact".into(), "proposed".into())
    );
    let again = import::auto_memory_scan(&conn, &projects).unwrap();
    assert_eq!((again.items, again.already), (0, 3));
}
