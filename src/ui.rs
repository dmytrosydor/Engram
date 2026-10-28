//! Local web UI on 127.0.0.1: what Claude gets, the review queue, the feed,
//! the map, the recall log and health.
//!
//! Server-rendered HTML with HTMX for partial updates; no frontend build.
//! Only localhost Host headers are served (no DNS rebinding) and every POST must
//! carry HTMX's `HX-Request` header, which a cross-site form cannot set.

use std::fmt::Write as _;
use std::path::PathBuf;

use anyhow::Result;
use axum::extract::{Form, Path, Query, Request};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rusqlite::{Connection, OptionalExtension};
use serde::Deserialize;
use serde_json::json;

use crate::memory::{self, Hit, Scope};
use crate::review::{self, NewProposal};
use crate::store::{self, KINDS};
use crate::{core, daily, db, embed, index, spaces};

const ACTOR: &str = "ui";

pub fn serve(port: u16) -> Result<()> {
    db::init(&db::db_path())?;
    let app = Router::new()
        .route("/", get(claude_view))
        .route("/core", get(core_fragment))
        .route("/search", get(search_fragment))
        .route("/queue", get(queue))
        .route("/review/{id}/accept", post(review_accept))
        .route("/review/{id}/reject", post(review_reject))
        .route("/review/{id}/edit", post(review_edit))
        .route("/feed", get(feed))
        .route("/item/{id}", get(item_page))
        .route("/item/{id}/edit", post(item_edit))
        .route("/item/{id}/{action}", post(item_action))
        .route("/map", get(map))
        .route("/graph", get(graph))
        .route("/recall", get(recall))
        .route("/health", get(health))
        .layer(middleware::from_fn(move |req, next| guard(req, next, port)));
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
        eprintln!("engram ui on http://127.0.0.1:{port}");
        axum::serve(listener, app).await?;
        Ok(())
    })
}

async fn guard(req: Request, next: Next, port: u16) -> Response {
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    let local = [format!("127.0.0.1:{port}"), format!("localhost:{port}")];
    if !local.iter().any(|h| h == host) {
        return (StatusCode::FORBIDDEN, "local only").into_response();
    }
    if req.method() == axum::http::Method::POST && !req.headers().contains_key("hx-request") {
        return (StatusCode::FORBIDDEN, "missing HX-Request").into_response();
    }
    next.run(req).await
}

/// Runs DB work off the runtime; errors render as a small red block.
async fn blocking(f: impl FnOnce(&Connection) -> Result<String> + Send + 'static) -> Html<String> {
    let out = tokio::task::spawn_blocking(move || -> Result<String> {
        let conn = db::connect(&db::db_path())?;
        f(&conn)
    })
    .await;
    Html(match out {
        Ok(Ok(html)) => html,
        Ok(Err(e)) => format!("<p class=err>{}</p>", esc(&format!("{e:#}"))),
        Err(e) => format!("<p class=err>{}</p>", esc(&e.to_string())),
    })
}

async fn page(
    title: &'static str,
    f: impl FnOnce(&Connection) -> Result<String> + Send + 'static,
) -> Html<String> {
    let Html(body) = blocking(f).await;
    Html(layout(title, &body))
}

fn layout(title: &str, body: &str) -> String {
    let nav = [
        ("/", "What Claude gets"),
        ("/queue", "Queue"),
        ("/feed", "Feed"),
        ("/map", "Map"),
        ("/recall", "Recall log"),
        ("/health", "Health"),
    ]
    .iter()
    .map(|(href, name)| {
        let cur = if *name == title { " class=cur" } else { "" };
        format!("<a href='{href}'{cur}>{name}</a>")
    })
    .collect::<Vec<_>>()
    .join("");
    format!(
        "<!doctype html><html lang=en><head><meta charset=utf-8>\
         <meta name=viewport content='width=device-width,initial-scale=1'>\
         <title>{t} · engram</title><style>{CSS}</style>\
         <script src='https://unpkg.com/htmx.org@2.0.4/dist/htmx.min.js'></script></head>\
         <body><nav><b>engram</b>{nav}</nav><main><h1>{t}</h1>{body}</main></body></html>",
        t = esc(title)
    )
}

const CSS: &str = "
:root{--bg:#fff;--fg:#1d1d1f;--mute:#6e6e73;--line:#e5e5ea;--acc:#0a66d8;--ok:#1a7f37;--bad:#c62828;--chip:#f2f2f7}
@media (prefers-color-scheme:dark){:root{--bg:#161618;--fg:#ececf0;--mute:#9a9aa2;--line:#2c2c30;--acc:#5aa2ff;--ok:#4cc26a;--bad:#ff6b6b;--chip:#232327}}
*{box-sizing:border-box}body{margin:0;background:var(--bg);color:var(--fg);font:14px/1.45 -apple-system,system-ui,sans-serif}
nav{display:flex;gap:4px;align-items:center;flex-wrap:wrap;padding:10px 16px;border-bottom:1px solid var(--line);position:sticky;top:0;background:var(--bg)}
nav b{margin-right:12px}nav a{color:var(--mute);text-decoration:none;padding:4px 8px;border-radius:6px}nav a.cur,nav a:hover{color:var(--fg);background:var(--chip)}
main{max-width:1000px;margin:0 auto;padding:8px 16px 48px}h1{font-size:20px}h2{font-size:16px;margin-top:24px}
a{color:var(--acc)}pre{white-space:pre-wrap;background:var(--chip);padding:12px;border-radius:8px;font:12px/1.5 ui-monospace,monospace;overflow-x:auto}
input,select,textarea{font:inherit;color:var(--fg);background:var(--bg);border:1px solid var(--line);border-radius:6px;padding:6px 8px}
textarea{width:100%;min-height:90px}input[type=text]{width:100%}
button{font:inherit;border:1px solid var(--line);background:var(--chip);color:var(--fg);border-radius:6px;padding:4px 10px;cursor:pointer}
button.ok{border-color:var(--ok);color:var(--ok)}button.bad{border-color:var(--bad);color:var(--bad)}
.row{border:1px solid var(--line);border-radius:8px;padding:10px 12px;margin:8px 0}
.meta{color:var(--mute);font-size:12px}.err{color:var(--bad)}.acts{display:flex;gap:6px;flex-wrap:wrap;margin-top:6px}
.grid{display:grid;grid-template-columns:repeat(auto-fill,minmax(220px,1fr));gap:8px}
table{border-collapse:collapse;width:100%}td,th{border-bottom:1px solid var(--line);padding:4px 6px;text-align:left;vertical-align:top}
.line{font:12px/1.6 ui-monospace,monospace}.form{display:flex;gap:8px;flex-wrap:wrap;align-items:end}.form>label{flex:1;min-width:200px}
#graph{height:520px;border:1px solid var(--line);border-radius:8px}
";

pub fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn hit_line(h: &Hit) -> String {
    format!(
        "<div class=line><a href='/item/{}'>{}</a></div>",
        h.id,
        esc(&h.line())
    )
}

// ---------- 1. what Claude gets ----------

#[derive(Deserialize)]
struct DirQ {
    dir: Option<String>,
}

async fn claude_view(Query(p): Query<DirQ>) -> Html<String> {
    let dir = p
        .dir
        .unwrap_or_else(|| std::env::var("HOME").unwrap_or_default());
    let body = format!(
        "<form class=form hx-get=/core hx-target=#core hx-trigger='load, submit' hx-include='[name=dir]'>\
         <label>Session folder<input type=text name=dir value='{d}'></label><button>Show core</button></form>\
         <div id=core></div>\
         <h2>Search as Claude sees it</h2>\
         <form class=form hx-get=/search hx-target=#hits hx-include='[name=dir]'>\
         <label>Query<input type=text name=q placeholder='e.g. git identity'></label>\
         <select name=scope><option>project</option><option>all</option><option>archive</option></select>\
         <button>Search</button></form><div id=hits></div>",
        d = esc(&dir)
    );
    Html(layout("What Claude gets", &body))
}

async fn core_fragment(Query(p): Query<DirQ>) -> Html<String> {
    blocking(move |conn| {
        let dir = PathBuf::from(spaces::expand_home(p.dir.as_deref().unwrap_or("~")));
        let space = spaces::resolve(conn, &dir)?;
        core::rebuild(conn, &space)?;
        let text = core::get(conn, &space)?;
        Ok(format!(
            "<p class=meta>space <b>{}</b> · sees {} · {} chars · ~{} tokens</p><pre>{}</pre>",
            esc(&space),
            esc(&spaces::visible(conn, &space)?.join(", ")),
            text.chars().count(),
            core::estimate_tokens(&text),
            esc(&text)
        ))
    })
    .await
}

#[derive(Deserialize)]
struct SearchQ {
    dir: Option<String>,
    q: String,
    scope: Option<String>,
}

async fn search_fragment(Query(p): Query<SearchQ>) -> Html<String> {
    blocking(move |conn| {
        let dir = PathBuf::from(spaces::expand_home(p.dir.as_deref().unwrap_or("~")));
        let space = spaces::resolve(conn, &dir)?;
        let visible = spaces::visible(conn, &space)?;
        let started = std::time::Instant::now();
        let vector = embed::embed_one(&p.q).ok();
        let hits = memory::search(
            conn,
            &memory::Query {
                text: &p.q,
                vector: vector.as_deref(),
                spaces: &visible,
                scope: Scope::parse(p.scope.as_deref())?,
                kind: None,
                limit: 8,
                as_of: None,
            },
        )?;
        let mut out = format!(
            "<p class=meta>{} results in {:?}{}</p>",
            hits.len(),
            started.elapsed(),
            if vector.is_none() {
                " · words only (model unavailable)"
            } else {
                ""
            }
        );
        for h in &hits {
            out.push_str(&hit_line(h));
        }
        Ok(out)
    })
    .await
}

// ---------- 2. review queue ----------

async fn queue() -> Html<String> {
    page("Queue", |conn| {
        let open = review::list(conn, "open", 200)?;
        if open.is_empty() {
            return Ok("<p class=meta>Queue is empty.</p>".into());
        }
        let mut out = format!("<p class=meta>{} proposals</p>", open.len());
        for e in open {
            // one broken proposal must not hide the rest of the queue
            match review_card(conn, &e) {
                Ok(card) => out.push_str(&card),
                Err(err) => write!(
                    out,
                    "<div class=row id=r{id}><p class=err>R{id}: {}</p><div class=acts>\
                     <button class=bad hx-post=/review/{id}/reject hx-target=#r{id} hx-swap=outerHTML>Reject</button></div></div>",
                    esc(&format!("{err:#}")),
                    id = e.id
                )?,
            }
        }
        Ok(out)
    })
    .await
}

fn review_card(conn: &Connection, e: &review::Entry) -> Result<String> {
    let id = e.id;
    let mut out = format!(
        "<div class=row id=r{id}><div class=meta>R{id} · {} · {}</div>",
        esc(&e.op),
        esc(e.reason.as_deref().unwrap_or(""))
    );
    match e.op.as_str() {
        "new" => {
            let p: NewProposal = serde_json::from_value(e.proposal.clone())?;
            let kinds: String = KINDS
                .iter()
                .map(|k| {
                    format!(
                        "<option{}>{k}</option>",
                        if *k == p.kind { " selected" } else { "" }
                    )
                })
                .collect();
            write!(
                out,
                "<form hx-post=/review/{id}/edit hx-target=#r{id} hx-swap=outerHTML>\
                 <div class=form><label>Title<input type=text name=title value='{t}'></label>\
                 <select name=kind>{kinds}</select><label>Space<input type=text name=space value='{s}'></label></div>\
                 <textarea name=body>{b}</textarea>\
                 <div class=acts><button class=ok>Yes (with edits)</button>\
                 <button class=bad hx-post=/review/{id}/reject hx-target=#r{id} hx-swap=outerHTML>No</button></div></form>",
                t = esc(&p.title),
                s = esc(&p.space),
                b = esc(&p.body)
            )?;
        }
        "merge" => {
            let keep = e.proposal["keep"].as_i64().unwrap_or(0);
            let drop = e.proposal["drop"].as_i64().unwrap_or(0);
            write!(
                out,
                "<div>keep {}</div><div>and fold into it {}</div>\
                 <div class=acts><button class=ok hx-post=/review/{id}/accept hx-target=#r{id} hx-swap=outerHTML>Yes, merge</button>\
                 <button class=bad hx-post=/review/{id}/reject hx-target=#r{id} hx-swap=outerHTML>No, different</button></div>",
                brief(conn, keep)?,
                brief(conn, drop)?
            )?;
        }
        "move" | "activate" => {
            let item = e.item_ids.first().copied().unwrap_or(0);
            let what = if e.op == "move" {
                format!(
                    "move to <b>{}</b>",
                    esc(e.proposal["space"].as_str().unwrap_or("?"))
                )
            } else {
                "make active (will appear in cores)".to_string()
            };
            write!(
                out,
                "<div>{what}: {}</div>\
                 <div class=acts><button class=ok hx-post=/review/{id}/accept hx-target=#r{id} hx-swap=outerHTML>Yes</button>\
                 <button class=bad hx-post=/review/{id}/reject hx-target=#r{id} hx-swap=outerHTML>No</button></div>",
                brief(conn, item)?
            )?;
        }
        other => write!(
            out,
            "<pre>{}</pre>",
            esc(&format!("{other}: {}", e.proposal))
        )?,
    }
    out.push_str("</div>");
    Ok(out)
}

fn brief(conn: &Connection, id: i64) -> Result<String> {
    let row: Option<(String, String, String)> = conn
        .query_row(
            "SELECT space_id, title, body FROM item WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    Ok(match row {
        Some((space, title, body)) => format!(
            "<a href='/item/{id}'>#{id}</a> <span class=meta>{}</span> <b>{}</b><div class=meta>{}</div>",
            esc(&space),
            esc(&title),
            esc(&store::clip(&body, 300))
        ),
        None => format!("#{id} (missing)"),
    })
}

async fn review_accept(Path(id): Path<i64>) -> Html<String> {
    blocking(move |conn| {
        Ok(format!(
            "<div class=row>R{id}: {}</div>",
            esc(&review::accept(conn, id, None, ACTOR)?)
        ))
    })
    .await
}

async fn review_reject(Path(id): Path<i64>) -> Html<String> {
    blocking(move |conn| {
        review::reject(conn, id)?;
        Ok(format!("<div class=row class=meta>R{id}: rejected</div>"))
    })
    .await
}

#[derive(Deserialize)]
struct Edited {
    title: String,
    body: String,
    kind: String,
    space: String,
}

async fn review_edit(Path(id): Path<i64>, Form(f): Form<Edited>) -> Html<String> {
    blocking(move |conn| {
        let p = NewProposal {
            kind: f.kind,
            title: f.title,
            body: f.body,
            space: f.space,
            source: None,
        };
        let source = review::list(conn, "open", 1000)?
            .into_iter()
            .find(|e| e.id == id)
            .and_then(|e| e.proposal["source"].as_str().map(str::to_string));
        let msg = review::accept(conn, id, Some(NewProposal { source, ..p }), ACTOR)?;
        Ok(format!("<div class=row>R{id}: {}</div>", esc(&msg)))
    })
    .await
}

// ---------- 3. feed ----------

#[derive(Deserialize)]
struct FeedQ {
    days: Option<u32>,
}

async fn feed(Query(p): Query<FeedQ>) -> Html<String> {
    let days = p.days.unwrap_or(2).clamp(1, 90);
    page("Feed", move |conn| {
        let mut stmt = conn.prepare(
            "SELECT id FROM item WHERE julianday('now') - julianday(COALESCE(updated_at, created_at)) <= ?1
             ORDER BY COALESCE(updated_at, created_at) DESC LIMIT 300",
        )?;
        let ids: Vec<i64> = stmt.query_map([days], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        let mut out = format!(
            "<p class=meta>{} records in {days} d · <a href='/feed?days=1'>1</a> · <a href='/feed?days=7'>7</a> · <a href='/feed?days=30'>30</a></p>",
            ids.len()
        );
        for id in ids {
            out.push_str(&item_card(conn, id)?);
        }
        Ok(out)
    })
    .await
}

fn item_card(conn: &Connection, id: i64) -> Result<String> {
    let (space, kind, status, pinned, created, title, body, source): (String, String, String, i64, String, String, String, Option<String>) =
        conn.query_row(
            "SELECT space_id, kind, status, pinned, COALESCE(updated_at, created_at), title, body, source FROM item WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?)),
        )?;
    let pin = if pinned == 1 {
        ("unpin", "Unpin")
    } else {
        ("pin", "Pin")
    };
    let life = if status == "archived" || status == "superseded" {
        ("restore", "Restore")
    } else {
        ("archive", "Archive")
    };
    Ok(format!(
        "<div class=row id=i{id}><div class=meta><a href='/item/{id}'>#{id}</a> · {kind} · {space} · {status}{p} · {created} · {src}</div>\
         <b>{title}</b><div>{body}</div><div class=acts>\
         <button hx-post=/item/{id}/{pa} hx-target=#i{id} hx-swap=outerHTML>{pl}</button>\
         <button hx-post=/item/{id}/{la} hx-target=#i{id} hx-swap=outerHTML>{ll}</button>\
         <a href='/item/{id}'>edit</a></div></div>",
        kind = esc(&kind),
        space = esc(&space),
        status = esc(&status),
        p = if pinned == 1 { " · 📌" } else { "" },
        created = esc(created.get(..16).unwrap_or(&created)),
        src = esc(source.as_deref().unwrap_or("")),
        title = esc(&title),
        body = esc(&store::clip(&body, 500)),
        pa = pin.0,
        pl = pin.1,
        la = life.0,
        ll = life.1,
    ))
}

async fn item_action(Path((id, action)): Path<(i64, String)>) -> Html<String> {
    blocking(move |conn| {
        match action.as_str() {
            "pin" => store::set_pinned(conn, id, true, ACTOR)?,
            "unpin" => store::set_pinned(conn, id, false, ACTOR)?,
            "archive" => store::set_status(conn, id, "archived", None, ACTOR)?,
            "restore" => {
                // superseded items stay retired: their replacement is the live one
                let status: String =
                    conn.query_row("SELECT status FROM item WHERE id = ?1", [id], |r| r.get(0))?;
                if status != "archived" {
                    anyhow::bail!("only archived items can be restored (this one is {status})");
                }
                store::set_status(conn, id, "active", None, ACTOR)?
            }
            other => anyhow::bail!("unknown action {other}"),
        }
        let space: String =
            conn.query_row("SELECT space_id FROM item WHERE id = ?1", [id], |r| {
                r.get(0)
            })?;
        core::rebuild(conn, &space)?;
        item_card(conn, id)
    })
    .await
}

async fn item_page(Path(id): Path<i64>) -> Html<String> {
    page("Record", move |conn| {
        let (space, kind, title, body): (String, String, String, String) = conn.query_row(
            "SELECT space_id, kind, title, body FROM item WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;
        let kinds: String = KINDS
            .iter()
            .map(|k| format!("<option{}>{k}</option>", if *k == kind { " selected" } else { "" }))
            .collect();
        let spaces_opts: String = spaces::all(conn)?
            .iter()
            .map(|s| format!("<option{}>{}</option>", if *s == space { " selected" } else { "" }, esc(s)))
            .collect();
        let mut out = item_card(conn, id)?;
        write!(
            out,
            "<h2>Edit</h2><form hx-post=/item/{id}/edit hx-target=#saved>\
             <div class=form><label>Title<input type=text name=title value='{t}'></label>\
             <select name=kind>{kinds}</select><select name=space>{spaces_opts}</select></div>\
             <textarea name=body rows=10>{b}</textarea><div class=acts><button class=ok>Save</button></div></form><div id=saved></div>",
            t = esc(&title),
            b = esc(&body)
        )?;
        // memory::get would count this view as a hit; the UI must not skew that signal
        let links = memory::links(conn, id)?;
        if !links.is_empty() {
            write!(out, "<h2>Links</h2><p>{}</p>", esc(&links.join(", ")))?;
        }
        out.push_str("<h2>History</h2><table><tr><th>when</th><th>who</th><th>what</th></tr>");
        let mut stmt = conn.prepare("SELECT at, actor, op FROM log WHERE item_id = ?1 ORDER BY id DESC LIMIT 50")?;
        for row in stmt.query_map([id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))? {
            let (at, actor, op) = row?;
            write!(out, "<tr><td>{}</td><td>{}</td><td>{}</td></tr>", esc(&at), esc(&actor), esc(&op))?;
        }
        out.push_str("</table>");
        Ok(out)
    })
    .await
}

async fn item_edit(Path(id): Path<i64>, Form(f): Form<Edited>) -> Html<String> {
    blocking(move |conn| {
        let title = f.title.trim();
        let body = f.body.trim();
        if title.is_empty()
            || body.is_empty()
            || title.chars().count() > store::TITLE_MAX
            || body.chars().count() > store::BODY_MAX
        {
            anyhow::bail!(
                "title must be 1–{} chars, body 1–{} chars",
                store::TITLE_MAX,
                store::BODY_MAX
            );
        }
        let old_space: String =
            conn.query_row("SELECT space_id FROM item WHERE id = ?1", [id], |r| {
                r.get(0)
            })?;
        let changed = store::edit(conn, id, title, body, &f.kind, &f.space, ACTOR)?;
        let note = if changed {
            match index::reembed_item(conn, id) {
                Ok(()) => "",
                Err(_) => " (the vector will be computed on the next reindex)",
            }
        } else {
            ""
        };
        core::rebuild(conn, &old_space)?;
        core::rebuild(conn, &f.space)?;
        Ok(format!("<p class=meta>Saved{note}.</p>"))
    })
    .await
}

// ---------- 4. map ----------

async fn map() -> Html<String> {
    page("Map", |conn| {
        let mut out = String::from("<div class=grid>");
        let mut stmt = conn.prepare(
            "SELECT s.id, s.domain, COALESCE(s.core_tokens, 0),
                    sum(i.status = 'active'), sum(i.status = 'proposed'), sum(i.status IN ('archived', 'superseded'))
             FROM space s LEFT JOIN item i ON i.space_id = s.id GROUP BY s.id ORDER BY s.domain, s.id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, Option<i64>>(3)?.unwrap_or(0),
                r.get::<_, Option<i64>>(4)?.unwrap_or(0),
                r.get::<_, Option<i64>>(5)?.unwrap_or(0),
            ))
        })?;
        for row in rows {
            let (id, domain, tokens, active, proposed, retired) = row?;
            write!(
                out,
                "<div class=row><b>{}</b><div class=meta>{} · core ~{tokens} tokens</div>\
                 <div>{active} active · {proposed} unreviewed · {retired} archived</div>\
                 <div class=acts><button data-space='{}' onclick=\"show(this.dataset.space)\">graph</button></div></div>",
                esc(&id),
                esc(&domain),
                esc(&id)
            )?;
        }
        out.push_str(
            "</div><h2>Confirmed links</h2><div id=graph></div>\
             <script src='https://unpkg.com/cytoscape@3.30.2/dist/cytoscape.min.js'></script>\
             <script>
             function show(space){fetch('/graph?space='+encodeURIComponent(space)).then(r=>r.json()).then(g=>{
               const dark=matchMedia('(prefers-color-scheme: dark)').matches;
               cytoscape({container:document.getElementById('graph'),elements:g,layout:{name:'cose',animate:false},
                 style:[{selector:'node',style:{label:'data(label)','font-size':8,'text-wrap':'ellipsis','text-max-width':120,
                   color:dark?'#ececf0':'#1d1d1f','background-color':'#0a66d8',width:10,height:10}},
                   {selector:'edge',style:{width:1,'line-color':'#9a9aa2','target-arrow-shape':'triangle','target-arrow-color':'#9a9aa2',
                   'curve-style':'bezier',label:'data(label)','font-size':6,color:'#9a9aa2'}}]})
               .on('tap','node',e=>location.href='/item/'+e.target.id());});}
             </script>",
        );
        Ok(out)
    })
    .await
}

#[derive(Deserialize)]
struct SpaceQ {
    space: String,
}

async fn graph(Query(p): Query<SpaceQ>) -> Json<serde_json::Value> {
    let out = tokio::task::spawn_blocking(move || -> Result<serde_json::Value> {
        let conn = db::connect(&db::db_path())?;
        let mut stmt = conn.prepare(
            "SELECT e.src, e.dst, e.type, a.title, b.title FROM edge e
             JOIN item a ON a.id = e.src JOIN item b ON b.id = e.dst
             WHERE e.confirmed = 1 AND (a.space_id = ?1 OR b.space_id = ?1) LIMIT 400",
        )?;
        let mut nodes = std::collections::BTreeMap::new();
        let mut edges = Vec::new();
        for row in stmt.query_map([&p.space], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?, r.get::<_, String>(4)?))
        })? {
            let (src, dst, t, ta, tb) = row?;
            nodes.insert(src, ta);
            nodes.insert(dst, tb);
            edges.push(json!({"data": {"id": format!("{src}-{dst}-{t}"), "source": src.to_string(), "target": dst.to_string(), "label": t}}));
        }
        let mut els: Vec<serde_json::Value> = nodes
            .into_iter()
            .map(|(id, title)| json!({"data": {"id": id.to_string(), "label": format!("#{id} {}", store::clip(&title, 40))}}))
            .collect();
        els.extend(edges);
        Ok(serde_json::Value::Array(els))
    })
    .await;
    Json(match out {
        Ok(Ok(v)) => v,
        _ => json!([]),
    })
}

// ---------- 5. recall log ----------

async fn recall() -> Html<String> {
    page("Recall log", |conn| {
        let mut out = String::from("<table><tr><th>when</th><th>space</th><th>query</th><th>returned</th><th>opened</th></tr>");
        let mut stmt = conn.prepare(
            "SELECT at, space_id, query, item_ids, COALESCE(opened, '[]') FROM recall ORDER BY id DESC LIMIT 100",
        )?;
        for row in stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?, r.get::<_, String>(4)?))
        })? {
            let (at, space, query, ids, opened) = row?;
            let link = |json: &str| -> String {
                serde_json::from_str::<Vec<i64>>(json)
                    .unwrap_or_default()
                    .iter()
                    .map(|i| format!("<a href='/item/{i}'>#{i}</a>"))
                    .collect::<Vec<_>>()
                    .join(" ")
            };
            write!(
                out,
                "<tr><td class=meta>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                esc(at.get(..16).unwrap_or(&at)),
                esc(&space),
                esc(&query),
                link(&ids),
                link(&opened)
            )?;
        }
        out.push_str("</table>");
        let (never, total): (i64, i64) = conn.query_row(
            "SELECT sum(hits = 0 AND julianday('now') - julianday(created_at) > 30), count(*)
             FROM item WHERE status = 'active'",
            [],
            |r| Ok((r.get::<_, Option<i64>>(0)?.unwrap_or(0), r.get(1)?)),
        )?;
        write!(
            out,
            "<h2>Never opened</h2><p class=meta>{never} of {total} active records are older than 30 days and were never opened \
             — archive candidates. Oldest:</p>"
        )?;
        let mut stmt = conn.prepare(
            "SELECT id, kind, space_id, status, created_at, title FROM item
             WHERE status = 'active' AND hits = 0 AND julianday('now') - julianday(created_at) > 30
             ORDER BY created_at LIMIT 30",
        )?;
        for h in stmt.query_map([], |r| {
            Ok(Hit {
                id: r.get(0)?,
                kind: r.get(1)?,
                space: r.get(2)?,
                status: r.get(3)?,
                created: r.get(4)?,
                title: r.get(5)?,
                score: 0.0,
            })
        })? {
            out.push_str(&hit_line(&h?));
        }
        Ok(out)
    })
    .await
}

// ---------- 6. health ----------

async fn health() -> Html<String> {
    page("Health", |conn| {
        let n = |sql: &str| -> Result<i64> { Ok(conn.query_row(sql, [], |r| r.get(0))?) };
        let rows = [
            (
                "Expired state (still active)",
                n("SELECT count(*) FROM item WHERE kind IN ('state','task') AND status IN ('active','proposed')
                   AND valid_until IS NOT NULL AND valid_until < date('now')")?,
            ),
            ("Proposals in queue", n("SELECT count(*) FROM review WHERE status = 'open'")?),
            ("Unreviewed records (proposed)", n("SELECT count(*) FROM item WHERE status = 'proposed'")?),
            ("Records without a vector", n("SELECT count(*) FROM item WHERE id NOT IN (SELECT item_id FROM item_vec)")?),
            ("Bodies longer than 1500 chars", n("SELECT count(*) FROM item WHERE status IN ('active','proposed') AND length(body) > 1500")?),
            ("Tags outside the vocabulary", n("SELECT count(*) FROM tag WHERE tag NOT IN (SELECT tag FROM tag_vocab)")?),
            ("Unprocessed inbox notes", n("SELECT count(*) FROM inbox WHERE processed_at IS NULL")?),
        ];
        let mut out = String::from("<table>");
        for (name, v) in rows {
            write!(out, "<tr><td>{name}</td><td><b>{v}</b></td></tr>")?;
        }
        let last = daily::meta(conn, "daily_last_run")?.unwrap_or_else(|| "never".into());
        write!(
            out,
            "<tr><td>Last daily pass</td><td><b>{}</b></td></tr>\
             <tr><td>Embedding model</td><td><b>{}</b></td></tr></table>",
            esc(&last),
            if embed::is_running() { "running" } else { "stopped (starts on demand)" }
        )?;
        out.push_str("<h2>Recent actions</h2><table>");
        let mut stmt = conn.prepare(
            "SELECT at, actor, op, item_id FROM log WHERE op NOT IN ('create', 'tombstone') ORDER BY id DESC LIMIT 30",
        )?;
        for row in stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, Option<i64>>(3)?))
        })? {
            let (at, actor, op, item) = row?;
            let link = item.map_or(String::new(), |i| format!("<a href='/item/{i}'>#{i}</a>"));
            write!(out, "<tr><td class=meta>{}</td><td>{}</td><td>{}</td><td>{link}</td></tr>", esc(&at), esc(&actor), esc(&op))?;
        }
        out.push_str("</table>");
        Ok(out)
    })
    .await
}
