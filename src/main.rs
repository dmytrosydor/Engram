use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Result;
use clap::{Parser, Subcommand};
use engram::{core, daily, db, embed, eval, import, index, memory, review, spaces};

#[derive(Parser)]
#[command(
    name = "engram",
    version,
    about = "Local long-term memory for Claude Code"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create the database (safe to re-run)
    Init,
    /// Show versions, tables and counts
    Info,
    /// Embed texts and print timing and pairwise cosine (starts llama-server if needed)
    Embed { texts: Vec<String> },
    /// Load spaces and folder rules from a TOML file
    Spaces { file: PathBuf },
    /// Print the space a folder maps to (default: current folder)
    Where { dir: Option<PathBuf> },
    /// Import from an existing memory store (sources are only read)
    #[command(subcommand)]
    Import(ImportCmd),
    /// Embed items and chunks that have no vector yet
    Reindex,
    /// Run the MCP server on stdio (Claude Code starts this)
    Mcp,
    /// Search as Claude would from a folder
    Search {
        query: String,
        /// Folder whose space to search from (default: current)
        #[arg(long)]
        dir: Option<PathBuf>,
        /// project, all or archive
        #[arg(long)]
        scope: Option<String>,
        #[arg(long)]
        kind: Option<String>,
        #[arg(long, default_value_t = 8)]
        limit: usize,
    },
    /// Print items in full
    Get { ids: Vec<i64> },
    /// Print the core a session in this folder would get (rebuilt first)
    Core {
        dir: Option<PathBuf>,
        /// Rebuild every space and print sizes instead
        #[arg(long)]
        all: bool,
    },
    /// Daily pass: expire state, queue merges, classify imports, read new transcripts
    Daily {
        /// Exit quietly unless the scheduled run is due (once a day, on AC power)
        #[arg(long)]
        if_due: bool,
        /// Unreviewed imported items to classify
        #[arg(long, default_value_t = 120)]
        classify: usize,
        /// Changed transcripts to read
        #[arg(long, default_value_t = 8)]
        transcripts: usize,
        /// Near-duplicate pairs to judge
        #[arg(long, default_value_t = 40)]
        pairs: usize,
    },
    /// Drop a note into the inbox for the next daily pass
    Add {
        text: String,
        /// Space for the note (default: the current folder's)
        #[arg(long)]
        space: Option<String>,
    },
    /// The review queue
    #[command(subcommand)]
    Review(ReviewCmd),
    /// Local web UI on 127.0.0.1
    Ui {
        #[arg(long, default_value_t = 7777)]
        port: u16,
    },
    /// Search quality on (query -> opened record) pairs from past sessions
    #[command(subcommand)]
    Eval(EvalCmd),
    /// Claude Code hook entry points (read the hook JSON on stdin)
    #[command(subcommand)]
    Hook(HookCmd),
}

#[derive(Subcommand)]
enum EvalCmd {
    /// Collect pairs from transcripts into a JSONL file
    Extract {
        #[arg(long, default_value = "~/.engram/eval/pairs.jsonl")]
        out: String,
    },
    /// Hit@5 and MRR per space, latency, and a domain-isolation check
    Run {
        #[arg(long, default_value = "~/.engram/eval/pairs.jsonl")]
        pairs: String,
        /// Write per-pair outcomes here as JSON
        #[arg(long)]
        rows: Option<PathBuf>,
        /// A personal folder and a work folder for the isolation check
        #[arg(long, num_args = 2, value_names = ["PERSONAL", "WORK"])]
        isolation: Option<Vec<PathBuf>>,
    },
}

#[derive(Subcommand)]
enum ReviewCmd {
    /// Open proposals
    List {
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    Accept {
        ids: Vec<i64>,
    },
    Reject {
        ids: Vec<i64>,
    },
}

#[derive(Subcommand)]
enum HookCmd {
    /// Print the space core
    SessionStart,
}

#[derive(Subcommand)]
enum ImportCmd {
    /// Memora SQLite database
    Memora {
        #[arg(long)]
        manifest: PathBuf,
        #[arg(long, default_value = "~/.local/share/memora/memories.db")]
        db: String,
    },
    /// Claude Code auto-memory folders
    AutoMemory {
        #[arg(long)]
        inventory: PathBuf,
        #[arg(long, default_value = "~/.claude/projects")]
        dir: String,
    },
}

fn main() -> Result<ExitCode> {
    match Cli::parse().cmd {
        Cmd::Init => init(),
        Cmd::Info => info(),
        Cmd::Embed { texts } => embed(&texts),
        Cmd::Spaces { file } => {
            let conn = open()?;
            let (s, p) = spaces::load(&conn, &file)?;
            println!("{s} spaces, {p} path rules");
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Where { dir } => {
            let dir = match dir {
                Some(d) => d,
                None => std::env::current_dir()?,
            };
            let conn = open()?;
            let space = spaces::resolve(&conn, &dir)?;
            println!(
                "{space}  (sees: {})",
                spaces::visible(&conn, &space)?.join(", ")
            );
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Import(cmd) => {
            let conn = open()?;
            spaces::ensure_builtin(&conn)?;
            let report = match cmd {
                ImportCmd::Memora { manifest, db } => {
                    import::memora(&conn, &PathBuf::from(spaces::expand_home(&db)), &manifest)?
                }
                ImportCmd::AutoMemory { inventory, dir } => import::auto_memory(
                    &conn,
                    &PathBuf::from(spaces::expand_home(&dir)),
                    &inventory,
                )?,
            };
            println!("{report:?}");
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Mcp => {
            engram::mcp::serve()?;
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Search {
            query,
            dir,
            scope,
            kind,
            limit,
        } => {
            let conn = open()?;
            let dir = match dir {
                Some(d) => d,
                None => std::env::current_dir()?,
            };
            let space = spaces::resolve(&conn, &dir)?;
            let visible = spaces::visible(&conn, &space)?;
            let started = std::time::Instant::now();
            let vector = embed::embed_one(&query).ok();
            let embedded = started.elapsed();
            let hits = memory::search(
                &conn,
                &memory::Query {
                    text: &query,
                    vector: vector.as_deref(),
                    spaces: &visible,
                    scope: memory::Scope::parse(scope.as_deref())?,
                    kind: kind.as_deref(),
                    limit,
                    as_of: None,
                },
            )?;
            for h in &hits {
                println!("{}", h.line());
            }
            eprintln!(
                "[{space}] embed {embedded:?}, total {:?}",
                started.elapsed()
            );
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Get { ids } => {
            let conn = open()?;
            println!("{}", memory::get(&conn, &ids, "cli")?);
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Core { dir, all } => {
            let conn = open()?;
            if all {
                for (space, chars) in core::rebuild_all(&conn)? {
                    println!("{space:<24} {chars:>5} chars");
                }
                return Ok(ExitCode::SUCCESS);
            }
            let dir = match dir {
                Some(d) => d,
                None => std::env::current_dir()?,
            };
            let space = spaces::resolve(&conn, &dir)?;
            core::rebuild(&conn, &space)?;
            let text = core::get(&conn, &space)?;
            println!("{text}");
            eprintln!(
                "[{space}] {} chars, ~{} tokens",
                text.chars().count(),
                core::estimate_tokens(&text)
            );
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Daily {
            if_due,
            classify,
            transcripts,
            pairs,
        } => {
            let conn = open()?;
            if if_due && let Some(why) = daily::not_due(&conn)? {
                eprintln!("daily: skipped ({why})");
                return Ok(ExitCode::SUCCESS);
            }
            let home = std::env::var("HOME").unwrap_or_default();
            let workdir = db::home().join("daily");
            let projects = PathBuf::from(format!("{home}/.claude/projects"));
            // the daily pass's own sessions are not persisted, but never read them anyway
            let own = projects.join(workdir.to_string_lossy().replace(['/', '.'], "-"));
            let llm = daily::ClaudeCli {
                model: std::env::var("ENGRAM_DAILY_MODEL").unwrap_or_else(|_| "sonnet".into()),
                workdir,
            };
            let opts = daily::Options {
                classify,
                transcripts,
                pairs,
                projects,
                inbox_dir: db::home().join("inbox"),
                exclude: vec![own],
            };
            let report = daily::run(&conn, &llm, &opts, |s| eprintln!("daily: {s}"))?;
            println!("{report:#?}");
            Ok(if report.errors.is_empty() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            })
        }
        Cmd::Add { text, space } => {
            let conn = open()?;
            let space = match space {
                Some(s) => s,
                None => spaces::resolve(&conn, &std::env::current_dir()?)?,
            };
            let id = daily::add_note(&conn, &space, &text, "note:cli")?;
            println!("inbox #{id} ({space}); processed by the next daily pass");
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Eval(EvalCmd::Extract { out }) => {
            let out = PathBuf::from(spaces::expand_home(&out));
            if let Some(dir) = out.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let projects = PathBuf::from(spaces::expand_home("~/.claude/projects"));
            println!(
                "{} pairs -> {}",
                eval::extract(&projects, &out)?,
                out.display()
            );
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Eval(EvalCmd::Run {
            pairs,
            rows,
            isolation,
        }) => {
            let conn = open()?;
            let pairs = PathBuf::from(spaces::expand_home(&pairs));
            let r = eval::run(&conn, &pairs)?;
            println!(
                "pairs {} (unreachable {}), latency p50 {:.0} ms p95 {:.0} ms",
                r.total, r.unreachable, r.p50_ms, r.p95_ms
            );
            for (space, s) in std::iter::once(("overall".to_string(), &r.overall))
                .chain(r.by_space.iter().map(|(k, v)| (k.clone(), v)))
            {
                println!(
                    "  {space:<22} n={:<4} hit@5 {:.1}%  mrr {:.3}",
                    s.pairs,
                    100.0 * s.hit5 as f64 / s.pairs.max(1) as f64,
                    s.mrr
                );
            }
            if let Some(path) = rows {
                std::fs::write(&path, serde_json::to_string_pretty(&r.rows)?)?;
            }
            if let Some(dirs) = isolation {
                let (q, leaks) = eval::isolation(&conn, &pairs, &dirs[0], &dirs[1])?;
                println!("isolation: {q} searches, {leaks} results from the other domain");
            }
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Ui { port } => {
            engram::ui::serve(port)?;
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Review(cmd) => {
            let conn = open()?;
            match cmd {
                ReviewCmd::List { limit } => {
                    for e in review::list(&conn, "open", limit)? {
                        println!(
                            "R{} {} {:?} {}  [{}]",
                            e.id,
                            e.op,
                            e.item_ids,
                            e.proposal,
                            e.reason.unwrap_or_default()
                        );
                    }
                }
                ReviewCmd::Accept { ids } => {
                    for id in ids {
                        println!("R{id}: {}", review::accept(&conn, id, None, "cli")?);
                    }
                }
                ReviewCmd::Reject { ids } => {
                    for id in ids {
                        review::reject(&conn, id)?;
                        println!("R{id}: rejected");
                    }
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Hook(HookCmd::SessionStart) => {
            engram::hook::session_start();
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Reindex => {
            let conn = open()?;
            let started = std::time::Instant::now();
            let (items, chunks) = index::embed_missing(&conn, |done, total| {
                if done % 256 < 16 || done == total {
                    eprintln!("  {done}/{total}");
                }
            })?;
            println!(
                "embedded {items} items, {chunks} chunks in {:?}",
                started.elapsed()
            );
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// Opens the database, creating it on first use.
fn open() -> Result<rusqlite::Connection> {
    let path = db::db_path();
    db::init(&path)?;
    db::connect(&path)
}

fn embed(texts: &[String]) -> Result<ExitCode> {
    let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
    let started = std::time::Instant::now();
    let vecs = embed::embed(&refs)?;
    println!(
        "{} vectors x {} in {:?}",
        vecs.len(),
        embed::DIM,
        started.elapsed()
    );
    for i in 1..vecs.len() {
        let cos = embed::cosine(&vecs[0], &vecs[i]);
        println!("cos(0,{i}) = {cos:.4}");
    }
    Ok(ExitCode::SUCCESS)
}

fn init() -> Result<ExitCode> {
    let path = db::db_path();
    db::init(&path)?;
    println!("{}", path.display());
    Ok(ExitCode::SUCCESS)
}

fn info() -> Result<ExitCode> {
    let path = db::db_path();
    if !path.exists() {
        eprintln!("no database at {}; run `engram init`", path.display());
        return Ok(ExitCode::FAILURE);
    }
    let conn = db::connect(&path)?;
    let (sqlite, vec): (String, String) =
        conn.query_row("SELECT sqlite_version(), vec_version()", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?;
    let schema: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    let journal: String = conn.query_row("PRAGMA journal_mode", [], |r| r.get(0))?;

    println!("db       {}", path.display());
    println!("sqlite   {sqlite}");
    println!("vec      {vec}");
    println!("schema   {schema} ({journal})");
    println!("tables   {}", db::tables(&conn)?.join(" "));
    println!(
        "embed    {}",
        if embed::is_running() {
            "running"
        } else {
            "stopped"
        }
    );

    let mut stmt = conn.prepare(
        "SELECT space_id, status, count(*) FROM item GROUP BY space_id, status ORDER BY space_id, status",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
        ))
    })?;
    for row in rows {
        let (space, status, n) = row?;
        println!("  {space:<24} {status:<11} {n}");
    }
    let (docs, chunks, vecs): (i64, i64, i64) = conn.query_row(
        "SELECT (SELECT count(*) FROM doc), (SELECT count(*) FROM chunk), (SELECT count(*) FROM item_vec)",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    println!("docs {docs}, chunks {chunks}, item vectors {vecs}");
    Ok(ExitCode::SUCCESS)
}
