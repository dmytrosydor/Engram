use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Result;
use clap::{Parser, Subcommand};
use engram::{core, db, embed, import, index, memory, spaces};

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
    /// Claude Code hook entry points (read the hook JSON on stdin)
    #[command(subcommand)]
    Hook(HookCmd),
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
