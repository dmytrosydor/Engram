//! Terminal UI. `engram` with no arguments opens it.
//!
//! The main screen is a graph: `global` in the middle, one hub per domain around it,
//! each domain's spaces fanned out from its hub. Node size follows the number of
//! records; lines between spaces are confirmed links. Enter opens a space's
//! records; other screens are the review queue and search.

use std::f64::consts::PI;
use std::time::Duration;

use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::canvas::{Canvas, Circle, Line as Edge};
use ratatui::widgets::{Block, BorderType, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};
use rusqlite::{Connection, OptionalExtension};

use crate::memory::{self, Hit, Scope};
use crate::{core, db, embed, review, spaces, store};

const ACTOR: &str = "tui";
const PALETTE: [Color; 6] = [
    Color::Cyan,
    Color::Magenta,
    Color::Green,
    Color::Yellow,
    Color::Blue,
    Color::LightRed,
];
const DIM: Color = Color::DarkGray;
const GLOBAL: Color = Color::LightYellow;

/// A domain hub or a space, placed in layout units (global at the origin).
#[derive(Debug, Clone)]
pub struct Node {
    pub id: String,
    pub domain: String,
    pub hub: bool,
    pub active: i64,
    pub proposed: i64,
    pub archived: i64,
    pub last: Option<String>,
    pub x: f64,
    pub y: f64,
    pub r: f64,
    pub color: Color,
}

impl Node {
    fn total(&self) -> i64 {
        self.active + self.proposed
    }

    fn label(&self) -> String {
        if self.hub || !self.id.contains('/') {
            self.id.clone()
        } else {
            let name = self.id.split_once('/').map_or(&*self.id, |(_, n)| n);
            name.trim_start_matches('_').to_string()
        }
    }
}

#[derive(Debug, Default)]
pub struct Graph {
    pub nodes: Vec<Node>,
    /// (from, to, confirmed links); weight 0 = structure (hub to space, global to hub)
    pub edges: Vec<(usize, usize, i64)>,
}

/// Reads spaces, counts and cross-space links, and lays them out.
pub fn load_graph(conn: &Connection) -> Result<Graph> {
    let mut stmt = conn.prepare(
        "SELECT s.id, s.domain,
                coalesce(sum(i.status = 'active'), 0), coalesce(sum(i.status = 'proposed'), 0),
                coalesce(sum(i.status IN ('archived', 'superseded')), 0),
                max(coalesce(i.updated_at, i.created_at))
         FROM space s LEFT JOIN item i ON i.space_id = s.id
         GROUP BY s.id ORDER BY s.domain, s.id",
    )?;
    let spaces: Vec<Node> = stmt
        .query_map([], |r| {
            Ok(Node {
                id: r.get(0)?,
                domain: r.get(1)?,
                hub: false,
                active: r.get(2)?,
                proposed: r.get(3)?,
                archived: r.get(4)?,
                last: r.get(5)?,
                x: 0.0,
                y: 0.0,
                r: 0.0,
                color: DIM,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    // empty spaces only clutter the picture; global always stays
    let spaces: Vec<Node> = spaces
        .into_iter()
        .filter(|s| s.id == "global" || s.active + s.proposed + s.archived > 0)
        .collect();

    let mut domains: Vec<String> = spaces
        .iter()
        .map(|s| s.domain.clone())
        .filter(|d| d != "global")
        .collect();
    domains.dedup();
    // the unsorted space sits on the ring like a domain of one, after the real ones
    domains.sort_by_key(|d| (d == spaces::INBOX, d.clone()));

    let mut g = Graph::default();
    let mut global = spaces
        .iter()
        .find(|s| s.id == "global")
        .cloned()
        .unwrap_or_else(|| Node {
            id: "global".into(),
            domain: "global".into(),
            hub: false,
            active: 0,
            proposed: 0,
            archived: 0,
            last: None,
            x: 0.0,
            y: 0.0,
            r: 0.0,
            color: GLOBAL,
        });
    global.color = GLOBAL;
    global.r = radius(global.total());
    g.nodes.push(global);

    let n = domains.len().max(1) as f64;
    let mut real = 0;
    for (k, domain) in domains.iter().enumerate() {
        // two domains sit left and right; more go around the circle
        let theta = PI + k as f64 * 2.0 * PI / n;
        let members: Vec<&Node> = spaces.iter().filter(|s| &s.domain == domain).collect();
        let color = if domain == spaces::INBOX {
            DIM
        } else {
            real += 1;
            PALETTE[(real - 1) % PALETTE.len()]
        };
        let lone = domain == spaces::INBOX && members.len() == 1;
        let (hx, hy) = (34.0 * theta.cos(), 34.0 * theta.sin());
        let hub = if lone {
            0
        } else {
            g.nodes.push(Node {
                id: domain.clone(),
                domain: domain.clone(),
                hub: true,
                active: members.iter().map(|m| m.active).sum(),
                proposed: members.iter().map(|m| m.proposed).sum(),
                archived: members.iter().map(|m| m.archived).sum(),
                last: members.iter().filter_map(|m| m.last.clone()).max(),
                x: hx,
                y: hy,
                r: 2.2,
                color,
            });
            let hub = g.nodes.len() - 1;
            g.edges.push((0, hub, 0));
            hub
        };
        let m = members.len() as f64;
        let spread = (0.7 * (m - 1.0)).min(3.0);
        for (j, s) in members.iter().enumerate() {
            let phi = if m > 1.0 {
                theta - spread / 2.0 + spread * j as f64 / (m - 1.0)
            } else {
                theta
            };
            let (x, y) = if lone {
                (hx, hy)
            } else {
                (hx + 32.0 * phi.cos(), hy + 32.0 * phi.sin())
            };
            g.nodes.push(Node {
                x,
                y,
                r: radius(s.total()),
                color,
                ..(*s).clone()
            });
            let i = g.nodes.len() - 1;
            g.edges.push((if lone { 0 } else { hub }, i, 0));
        }
    }

    let mut stmt = conn.prepare(
        "SELECT a.space_id, b.space_id, count(*) FROM edge e
         JOIN item a ON a.id = e.src JOIN item b ON b.id = e.dst
         WHERE e.confirmed = 1 AND a.space_id <> b.space_id GROUP BY 1, 2",
    )?;
    let links: Vec<(String, String, i64)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let at = |id: &str, g: &Graph| g.nodes.iter().position(|n| !n.hub && n.id == id);
    for (a, b, n) in links {
        if let (Some(a), Some(b)) = (at(&a, &g), at(&b, &g)) {
            g.edges.push((a, b, n));
        }
    }
    Ok(g)
}

fn radius(records: i64) -> f64 {
    1.6 + (records.max(0) as f64).sqrt() * 0.2
}

/// The node nearest to `from` in a direction (dx, dy), within 60° of it.
fn step(g: &Graph, from: usize, dx: f64, dy: f64) -> usize {
    let (ox, oy) = (g.nodes[from].x, g.nodes[from].y);
    g.nodes
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != from)
        .filter_map(|(i, n)| {
            let (vx, vy) = (n.x - ox, n.y - oy);
            let d = (vx * vx + vy * vy).sqrt();
            let cos = (vx * dx + vy * dy) / d.max(1e-9);
            (cos > 0.5).then_some((i, d * (2.0 - cos)))
        })
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map_or(from, |(i, _)| i)
}

#[derive(Clone, Copy, PartialEq)]
enum Screen {
    Graph,
    Space,
    Queue,
    Search,
}

struct Row {
    id: i64,
    kind: String,
    status: String,
    pinned: bool,
    date: String,
    title: String,
}

pub struct App {
    conn: Connection,
    screen: Screen,
    graph: Graph,
    sel: usize,
    space: String,
    rows: Vec<Row>,
    rows_state: ListState,
    queue: Vec<review::Entry>,
    queue_state: ListState,
    query: String,
    hits: Vec<Hit>,
    hits_state: ListState,
    status: String,
    quit: bool,
}

pub fn run() -> Result<()> {
    let path = db::db_path();
    db::init(&path)?;
    let mut app = App::new(db::connect(&path)?)?;
    let mut terminal = ratatui::init();
    let result = app.event_loop(&mut terminal);
    ratatui::restore();
    result
}

impl App {
    pub fn new(conn: Connection) -> Result<Self> {
        let graph = load_graph(&conn)?;
        // start on the biggest space, the one most worth looking at
        let sel = (0..graph.nodes.len())
            .filter(|&i| !graph.nodes[i].hub)
            .max_by_key(|&i| graph.nodes[i].total())
            .unwrap_or(0);
        let mut app = Self {
            conn,
            screen: Screen::Graph,
            graph,
            sel,
            space: String::new(),
            rows: Vec::new(),
            rows_state: ListState::default(),
            queue: Vec::new(),
            queue_state: ListState::default(),
            query: String::new(),
            hits: Vec::new(),
            hits_state: ListState::default(),
            status: String::new(),
            quit: false,
        };
        app.load_queue()?;
        Ok(app)
    }

    fn event_loop(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        while !self.quit {
            terminal.draw(|f| self.draw(f))?;
            if event::poll(Duration::from_millis(250))?
                && let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
            {
                if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                    break;
                }
                if let Err(e) = self.on_key(key.code) {
                    self.status = format!("error: {e:#}");
                }
            }
        }
        Ok(())
    }

    /// Feeds one key press, as the event loop does (tests drive the UI this way).
    pub fn press(&mut self, key: KeyCode) -> Result<()> {
        self.on_key(key)
    }

    fn on_key(&mut self, key: KeyCode) -> Result<()> {
        if self.screen == Screen::Search && self.typing(key)? {
            return Ok(());
        }
        match key {
            KeyCode::Char('q') if self.screen != Screen::Search => {
                self.quit = true;
                return Ok(());
            }
            KeyCode::Char('1') => self.screen = Screen::Graph,
            KeyCode::Char('2') => self.screen = Screen::Queue,
            KeyCode::Char('/') | KeyCode::Char('3') => self.screen = Screen::Search,
            KeyCode::Tab => {
                self.screen = match self.screen {
                    Screen::Graph | Screen::Space => Screen::Queue,
                    Screen::Queue => Screen::Search,
                    Screen::Search => Screen::Graph,
                }
            }
            _ => {}
        }
        match self.screen {
            Screen::Graph => self.graph_key(key),
            Screen::Space => self.space_key(key),
            Screen::Queue => self.queue_key(key),
            Screen::Search => self.search_key(key),
        }
    }

    // ---------- graph ----------

    fn graph_key(&mut self, key: KeyCode) -> Result<()> {
        let dir = match key {
            KeyCode::Left | KeyCode::Char('h') => Some((-1.0, 0.0)),
            KeyCode::Right | KeyCode::Char('l') => Some((1.0, 0.0)),
            KeyCode::Up | KeyCode::Char('k') => Some((0.0, 1.0)),
            KeyCode::Down | KeyCode::Char('j') => Some((0.0, -1.0)),
            _ => None,
        };
        if let Some((dx, dy)) = dir {
            self.sel = step(&self.graph, self.sel, dx, dy);
        }
        if key == KeyCode::Enter {
            let node = &self.graph.nodes[self.sel];
            if node.hub {
                // a hub opens its biggest space
                let domain = node.domain.clone();
                if let Some(i) = (0..self.graph.nodes.len())
                    .filter(|&i| !self.graph.nodes[i].hub && self.graph.nodes[i].domain == domain)
                    .max_by_key(|&i| self.graph.nodes[i].total())
                {
                    self.sel = i;
                }
            }
            let space = self.graph.nodes[self.sel].id.clone();
            self.open_space(&space)?;
        }
        Ok(())
    }

    fn draw_graph(&self, f: &mut Frame, area: Rect) {
        let [canvas, side] =
            Layout::horizontal([Constraint::Min(40), Constraint::Length(38)]).areas(area);
        let g = &self.graph;
        // fit the layout into the area: braille dots are square when x spans
        // inner_w / (2 * inner_h) times the y range; leave room for labels at the sides
        let ex = g.nodes.iter().map(|n| n.x.abs() + n.r).fold(10.0, f64::max);
        let ey = g.nodes.iter().map(|n| n.y.abs() + n.r).fold(10.0, f64::max);
        let inner_w = canvas.width.saturating_sub(2).max(30) as f64;
        let inner_h = canvas.height.saturating_sub(2).max(5) as f64;
        let aspect = inner_w / (2.0 * inner_h);
        let half_x_needed = ex / (1.0 - 30.0 / inner_w).max(0.3);
        let half_y = (ey * 1.12).max(half_x_needed / aspect);
        let half_x = half_y * aspect;
        let cell = 2.0 * half_x / inner_w;
        let sel = self.sel;
        // stretch the picture vertically to use a tall window; circles stay round
        let ymax = g.nodes.iter().map(|n| n.y.abs()).fold(1.0, f64::max);
        let ky = ((half_y * 0.85 - 8.0) / ymax).clamp(1.0, 3.0);
        let widget = Canvas::default()
            .block(panel(" memory map "))
            .marker(Marker::Braille)
            .x_bounds([-half_x, half_x])
            .y_bounds([-half_y, half_y])
            .paint(move |ctx| {
                for &(a, b, w) in &g.edges {
                    let (na, nb) = (&g.nodes[a], &g.nodes[b]);
                    let color = if w > 0 { Color::Gray } else { dim(nb.color) };
                    ctx.draw(&Edge {
                        x1: na.x,
                        y1: na.y * ky,
                        x2: nb.x,
                        y2: nb.y * ky,
                        color,
                    });
                }
                ctx.layer();
                for (i, n) in g.nodes.iter().enumerate() {
                    let mut r = 0.3;
                    while r <= n.r {
                        ctx.draw(&Circle {
                            x: n.x,
                            y: n.y * ky,
                            radius: r,
                            color: n.color,
                        });
                        r += 0.35;
                    }
                    if i == sel {
                        for ring in [n.r + 1.2, n.r + 1.6] {
                            ctx.draw(&Circle {
                                x: n.x,
                                y: n.y * ky,
                                radius: ring,
                                color: Color::White,
                            });
                        }
                    }
                }
                for (i, n) in g.nodes.iter().enumerate() {
                    let name = n.label();
                    let count = if n.hub {
                        String::new()
                    } else {
                        format!(" {}", n.total())
                    };
                    let width = (name.chars().count() + count.len()) as f64 * cell;
                    let style = if i == sel {
                        Style::new().fg(Color::White).add_modifier(Modifier::BOLD)
                    } else if n.hub {
                        Style::new().fg(n.color).add_modifier(Modifier::BOLD)
                    } else {
                        Style::new().fg(n.color)
                    };
                    let line = Line::from(vec![
                        Span::styled(name, style),
                        Span::styled(count, Style::new().fg(DIM)),
                    ]);
                    // labels point away from the centre so they do not cover the hubs
                    let x = if n.x < -0.5 {
                        n.x - n.r - cell - width
                    } else if n.x > 0.5 {
                        n.x + n.r + cell
                    } else {
                        n.x - width / 2.0
                    };
                    let y = if n.x.abs() <= 0.5 {
                        n.y * ky - n.r - 2.0
                    } else {
                        n.y * ky
                    };
                    ctx.print(x, y, line);
                }
            });
        f.render_widget(widget, canvas);
        f.render_widget(self.node_panel(), side);
    }

    fn node_panel(&self) -> Paragraph<'_> {
        let n = &self.graph.nodes[self.sel];
        let mut lines = vec![
            Line::from(Span::styled(
                n.id.clone(),
                Style::new().fg(n.color).add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                if n.hub {
                    format!("domain · {} spaces", self.domain_size(&n.domain))
                } else {
                    format!("space in {}", n.domain)
                },
                Style::new().fg(DIM),
            )),
            Line::raw(""),
        ];
        let max = n.active.max(n.proposed).max(n.archived).max(1);
        for (label, v, color) in [
            ("active  ", n.active, n.color),
            ("unreviewed", n.proposed, Color::Yellow),
            ("archived", n.archived, DIM),
        ] {
            lines.push(Line::from(vec![
                Span::styled(format!("{label:<11}"), Style::new().fg(DIM)),
                Span::styled(bar(v, max, 14), Style::new().fg(color)),
                Span::raw(format!(" {v}")),
            ]));
        }
        lines.push(Line::raw(""));
        if let Some(last) = &n.last {
            lines.push(Line::from(vec![
                Span::styled("last write ", Style::new().fg(DIM)),
                Span::raw(last.get(..10).unwrap_or(last).to_string()),
            ]));
        }
        if !n.hub
            && let Ok(v) = spaces::visible(&self.conn, &n.id)
        {
            lines.push(Line::from(vec![
                Span::styled("sees       ", Style::new().fg(DIM)),
                Span::raw(v[1..].join(", ")),
            ]));
        }
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            "recent",
            Style::new().fg(DIM).add_modifier(Modifier::BOLD),
        )));
        for title in self.recent_titles(n).unwrap_or_default() {
            lines.push(Line::raw(format!("· {title}")));
        }
        Paragraph::new(lines)
            .block(panel(" selected "))
            .wrap(Wrap { trim: true })
    }

    fn domain_size(&self, domain: &str) -> usize {
        self.graph
            .nodes
            .iter()
            .filter(|n| !n.hub && n.domain == domain)
            .count()
    }

    fn recent_titles(&self, n: &Node) -> Result<Vec<String>> {
        let (sql, arg) = if n.hub {
            (
                "SELECT i.title FROM item i JOIN space s ON s.id = i.space_id
                 WHERE s.domain = ?1 AND i.status IN ('active','proposed')
                 ORDER BY coalesce(i.updated_at, i.created_at) DESC LIMIT 6",
                n.domain.clone(),
            )
        } else {
            (
                "SELECT title FROM item WHERE space_id = ?1 AND status IN ('active','proposed')
                 ORDER BY coalesce(updated_at, created_at) DESC LIMIT 6",
                n.id.clone(),
            )
        };
        let mut stmt = self.conn.prepare(sql)?;
        let titles = stmt
            .query_map([arg], |r| r.get::<_, String>(0))?
            .map(|t| t.map(|t| store::clip(tidy(&t), 60)))
            .collect::<rusqlite::Result<_>>()?;
        Ok(titles)
    }

    // ---------- one space ----------

    fn open_space(&mut self, space: &str) -> Result<()> {
        self.space = space.to_string();
        let mut stmt = self.conn.prepare(
            "SELECT id, kind, status, pinned, coalesce(updated_at, created_at), title FROM item
             WHERE space_id = ?1
             ORDER BY status IN ('active','proposed') DESC, pinned DESC,
                      coalesce(updated_at, created_at) DESC LIMIT 1000",
        )?;
        self.rows = stmt
            .query_map([space], |r| {
                Ok(Row {
                    id: r.get(0)?,
                    kind: r.get(1)?,
                    status: r.get(2)?,
                    pinned: r.get::<_, i64>(3)? == 1,
                    date: r.get(4)?,
                    title: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        let keep = self.rows_state.selected().filter(|&i| i < self.rows.len());
        self.rows_state
            .select(keep.or((!self.rows.is_empty()).then_some(0)));
        self.screen = Screen::Space;
        Ok(())
    }

    fn space_key(&mut self, key: KeyCode) -> Result<()> {
        move_in(&mut self.rows_state, self.rows.len(), key);
        let Some(row) = self.rows_state.selected().and_then(|i| self.rows.get(i)) else {
            if key == KeyCode::Esc {
                self.screen = Screen::Graph;
            }
            return Ok(());
        };
        let id = row.id;
        match key {
            KeyCode::Esc | KeyCode::Backspace => self.screen = Screen::Graph,
            KeyCode::Char('p') => {
                store::set_pinned(&self.conn, id, !row.pinned, ACTOR)?;
                self.status = format!("#{id} {}", if row.pinned { "unpinned" } else { "pinned" });
                self.after_change()?;
            }
            KeyCode::Char('a') => {
                let to = if row.status == "archived" {
                    "active"
                } else {
                    "archived"
                };
                if row.status == "superseded" {
                    self.status = format!("#{id} was replaced by a newer record; it stays retired");
                } else {
                    store::set_status(&self.conn, id, to, None, ACTOR)?;
                    self.status = format!("#{id} {to}");
                    self.after_change()?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Cores and counts change with every edit.
    fn after_change(&mut self) -> Result<()> {
        let space = self.space.clone();
        core::rebuild(&self.conn, &space)?;
        let selected = self.graph.nodes[self.sel].id.clone();
        self.graph = load_graph(&self.conn)?;
        self.sel = self
            .graph
            .nodes
            .iter()
            .position(|n| n.id == selected)
            .unwrap_or(0);
        if self.screen == Screen::Space {
            self.open_space(&space)?;
        }
        self.load_queue()
    }

    fn draw_space(&mut self, f: &mut Frame, area: Rect) {
        let [list, preview] =
            Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)])
                .areas(area);
        let color = self
            .graph
            .nodes
            .iter()
            .find(|n| n.id == self.space)
            .map_or(Color::White, |n| n.color);
        let items: Vec<ListItem> = self
            .rows
            .iter()
            .map(|r| {
                let live = r.status == "active" || r.status == "proposed";
                let style = if live {
                    Style::new()
                } else {
                    Style::new().fg(DIM)
                };
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{} ", kind_letter(&r.kind)), Style::new().fg(color)),
                    Span::styled(
                        if r.pinned {
                            "● "
                        } else if r.status == "proposed" {
                            "? "
                        } else {
                            "  "
                        },
                        Style::new().fg(Color::Yellow),
                    ),
                    Span::styled(tidy(&r.title).to_string(), style),
                    Span::styled(
                        format!("  {}", r.date.get(5..10).unwrap_or("")),
                        Style::new().fg(DIM),
                    ),
                ]))
            })
            .collect();
        let title = format!(" {} · {} records ", self.space, self.rows.len());
        let widget = List::new(items)
            .block(panel(&title))
            .highlight_style(
                Style::new()
                    .bg(Color::Rgb(40, 44, 52))
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("▌");
        f.render_stateful_widget(widget, list, &mut self.rows_state);
        let id = self
            .rows_state
            .selected()
            .and_then(|i| self.rows.get(i))
            .map(|r| r.id);
        f.render_widget(self.record(id, color), preview);
    }

    /// Full record without counting it as a recall hit (that signal is Claude's).
    fn record(&self, id: Option<i64>, color: Color) -> Paragraph<'_> {
        let Some(id) = id else {
            return Paragraph::new("nothing here").block(panel(" record "));
        };
        type Full = (
            String,
            String,
            String,
            String,
            String,
            Option<String>,
            String,
        );
        let row: Option<Full> = self
            .conn
            .query_row(
                "SELECT space_id, kind, status, coalesce(updated_at, created_at), title, source, body
                 FROM item WHERE id = ?1",
                [id],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                    ))
                },
            )
            .optional()
            .ok()
            .flatten();
        let Some((space, kind, status, date, title, source, body)) = row else {
            return Paragraph::new("not found").block(panel(" record "));
        };
        let mut text = Text::default();
        text.push_line(Line::styled(
            title,
            Style::new().fg(color).add_modifier(Modifier::BOLD),
        ));
        text.push_line(Line::styled(
            format!(
                "#{id} · {kind} · {status} · {space} · {}{}",
                date.get(..10).unwrap_or(&date),
                source.map(|s| format!(" · {s}")).unwrap_or_default()
            ),
            Style::new().fg(DIM),
        ));
        text.push_line(Line::raw(""));
        for line in body.lines() {
            text.push_line(Line::raw(line.to_string()));
        }
        if let Ok(links) = memory::links(&self.conn, id, None)
            && !links.is_empty()
        {
            text.push_line(Line::raw(""));
            text.push_line(Line::styled(
                format!("links: {}", links.join(", ")),
                Style::new().fg(DIM),
            ));
        }
        Paragraph::new(text)
            .block(panel(" record "))
            .wrap(Wrap { trim: false })
    }

    // ---------- review queue ----------

    fn load_queue(&mut self) -> Result<()> {
        let mut q = review::list(&self.conn, "open", 500)?;
        // anything heading for `global` reaches every session: those come first
        q.sort_by_key(|e| (!touches_global(e), e.id));
        self.queue = q;
        let keep = self
            .queue_state
            .selected()
            .filter(|&i| i < self.queue.len());
        self.queue_state
            .select(keep.or((!self.queue.is_empty()).then_some(0)));
        Ok(())
    }

    fn queue_key(&mut self, key: KeyCode) -> Result<()> {
        move_in(&mut self.queue_state, self.queue.len(), key);
        let Some(e) = self.queue_state.selected().and_then(|i| self.queue.get(i)) else {
            return Ok(());
        };
        let id = e.id;
        match key {
            KeyCode::Char('y') => {
                self.status = format!("R{id}: {}", review::accept(&self.conn, id, None, ACTOR)?);
                self.after_review()?;
            }
            KeyCode::Char('n') => {
                review::reject(&self.conn, id)?;
                self.status = format!("R{id}: rejected");
                self.after_review()?;
            }
            KeyCode::Esc => self.screen = Screen::Graph,
            _ => {}
        }
        Ok(())
    }

    fn after_review(&mut self) -> Result<()> {
        let selected = self.graph.nodes[self.sel].id.clone();
        self.graph = load_graph(&self.conn)?;
        self.sel = self
            .graph
            .nodes
            .iter()
            .position(|n| n.id == selected)
            .unwrap_or(0);
        self.load_queue()
    }

    fn draw_queue(&mut self, f: &mut Frame, area: Rect) {
        let [list, card] =
            Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)])
                .areas(area);
        let items: Vec<ListItem> = self
            .queue
            .iter()
            .map(|e| {
                let (tag, color) = match e.op.as_str() {
                    _ if touches_global(e) => ("global", GLOBAL),
                    "merge" => ("merge ", Color::Cyan),
                    "new" => ("new   ", Color::Green),
                    other => (other, Color::White),
                };
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{tag} "), Style::new().fg(color)),
                    Span::raw(self.entry_title(e)),
                ]))
            })
            .collect();
        let globals = self.queue.iter().filter(|e| touches_global(e)).count();
        let title = format!(" queue · {} open · {globals} for global ", self.queue.len());
        let widget = List::new(items)
            .block(panel(&title))
            .highlight_style(
                Style::new()
                    .bg(Color::Rgb(40, 44, 52))
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("▌");
        f.render_stateful_widget(widget, list, &mut self.queue_state);
        let detail = self
            .queue_state
            .selected()
            .and_then(|i| self.queue.get(i))
            .map(|e| self.entry_card(e))
            .unwrap_or_else(|| Text::raw("The queue is empty."));
        f.render_widget(
            Paragraph::new(detail)
                .block(panel(" proposal "))
                .wrap(Wrap { trim: false }),
            card,
        );
    }

    fn entry_title(&self, e: &review::Entry) -> String {
        let t = match e.op.as_str() {
            "new" => e.proposal["title"].as_str().unwrap_or("").to_string(),
            "merge" => self.title_of(e.proposal["keep"].as_i64().unwrap_or(0)),
            _ => self.title_of(e.item_ids.first().copied().unwrap_or(0)),
        };
        store::clip(tidy(&t), 70)
    }

    fn title_of(&self, id: i64) -> String {
        self.conn
            .query_row("SELECT title FROM item WHERE id = ?1", [id], |r| r.get(0))
            .unwrap_or_else(|_| format!("#{id} (gone)"))
    }

    fn brief(&self, id: i64) -> Vec<Line<'static>> {
        let row: Option<(String, String, String)> = self
            .conn
            .query_row(
                "SELECT space_id, title, body FROM item WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .ok()
            .flatten();
        match row {
            Some((space, title, body)) => {
                let mut out = vec![
                    Line::styled(
                        format!("#{id} {}", tidy(&title)),
                        Style::new().add_modifier(Modifier::BOLD),
                    ),
                    Line::styled(space, Style::new().fg(DIM)),
                ];
                out.extend(
                    store::clip(&body, 500)
                        .lines()
                        .map(|l| Line::raw(l.to_string())),
                );
                out
            }
            None => vec![Line::raw(format!("#{id} is gone"))],
        }
    }

    fn entry_card(&self, e: &review::Entry) -> Text<'static> {
        let mut t = Text::default();
        let reason = e.reason.clone().unwrap_or_default();
        match e.op.as_str() {
            "new" => {
                let p = &e.proposal;
                let space = p["space"].as_str().unwrap_or("?").to_string();
                t.push_line(Line::styled(
                    format!("save as {} in {space}", p["kind"].as_str().unwrap_or("?")),
                    Style::new().fg(if space == "global" {
                        GLOBAL
                    } else {
                        Color::Green
                    }),
                ));
                t.push_line(Line::raw(""));
                t.push_line(Line::styled(
                    p["title"].as_str().unwrap_or("").to_string(),
                    Style::new().add_modifier(Modifier::BOLD),
                ));
                for line in p["body"].as_str().unwrap_or("").lines() {
                    t.push_line(Line::raw(line.to_string()));
                }
            }
            "merge" => {
                t.push_line(Line::styled(
                    "the same thing twice: keep the first, fold the second into it",
                    Style::new().fg(Color::Cyan),
                ));
                t.push_line(Line::raw(""));
                for l in self.brief(e.proposal["keep"].as_i64().unwrap_or(0)) {
                    t.push_line(l);
                }
                t.push_line(Line::raw(""));
                for l in self.brief(e.proposal["drop"].as_i64().unwrap_or(0)) {
                    t.push_line(l);
                }
            }
            op => {
                let what = if op == "move" {
                    format!(
                        "move to {} — every session would see it",
                        e.proposal["space"].as_str().unwrap_or("?")
                    )
                } else {
                    "make active — it would reach every session's core".into()
                };
                t.push_line(Line::styled(what, Style::new().fg(GLOBAL)));
                t.push_line(Line::raw(""));
                for l in self.brief(e.item_ids.first().copied().unwrap_or(0)) {
                    t.push_line(l);
                }
            }
        }
        if !reason.is_empty() {
            t.push_line(Line::raw(""));
            t.push_line(Line::styled(format!("why: {reason}"), Style::new().fg(DIM)));
        }
        t
    }

    // ---------- search ----------

    /// Typing goes into the query; returns whether the key was consumed.
    fn typing(&mut self, key: KeyCode) -> Result<bool> {
        match key {
            KeyCode::Char(c) => {
                self.query.push(c);
                Ok(true)
            }
            KeyCode::Backspace => {
                self.query.pop();
                Ok(true)
            }
            KeyCode::Enter => {
                self.search()?;
                Ok(true)
            }
            KeyCode::Esc if !self.query.is_empty() => {
                self.query.clear();
                self.hits.clear();
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// Searches as a session in the selected space would (a hub: its whole domain).
    fn search(&mut self) -> Result<()> {
        let node = &self.graph.nodes[self.sel];
        let (space, scope) = if node.hub {
            (format!("{}/_shared", node.domain), Scope::All)
        } else {
            (node.id.clone(), Scope::Project)
        };
        let visible = memory::spaces_for(&self.conn, &space, scope)
            .or_else(|_| memory::spaces_for(&self.conn, "global", Scope::Project))?;
        let vector = embed::embed_one(&self.query).ok();
        self.hits = memory::search(
            &self.conn,
            &memory::Query {
                text: &self.query,
                vector: vector.as_deref(),
                spaces: &visible,
                scope,
                kind: None,
                limit: 30,
                as_of: None,
            },
        )?;
        self.status = format!(
            "{} results as {}{}",
            self.hits.len(),
            node.id,
            if vector.is_none() {
                " · words only"
            } else {
                ""
            }
        );
        self.hits_state.select((!self.hits.is_empty()).then_some(0));
        Ok(())
    }

    fn search_key(&mut self, key: KeyCode) -> Result<()> {
        move_in(&mut self.hits_state, self.hits.len(), key);
        if key == KeyCode::Esc {
            self.screen = Screen::Graph;
        }
        Ok(())
    }

    fn draw_search(&mut self, f: &mut Frame, area: Rect) {
        let [input, body] =
            Layout::vertical([Constraint::Length(3), Constraint::Min(5)]).areas(area);
        let node = &self.graph.nodes[self.sel];
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("› ", Style::new().fg(node.color)),
                Span::raw(self.query.clone()),
                Span::styled("▏", Style::new().fg(Color::White)),
            ]))
            .block(panel(&format!(" search as {} ", node.id))),
            input,
        );
        let [list, preview] =
            Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)])
                .areas(body);
        let color_of = |space: &str| {
            self.graph
                .nodes
                .iter()
                .find(|n| n.id == space)
                .map_or(Color::White, |n| n.color)
        };
        let items: Vec<ListItem> = self
            .hits
            .iter()
            .map(|h| {
                ListItem::new(Line::from(vec![
                    Span::styled(
                        format!("{} ", kind_letter(&h.kind)),
                        Style::new().fg(color_of(&h.space)),
                    ),
                    Span::raw(store::clip(tidy(&h.title), 70)),
                    Span::styled(format!("  {}", h.space), Style::new().fg(DIM)),
                ]))
            })
            .collect();
        f.render_stateful_widget(
            List::new(items)
                .block(panel(" results "))
                .highlight_style(
                    Style::new()
                        .bg(Color::Rgb(40, 44, 52))
                        .add_modifier(Modifier::BOLD),
                )
                .highlight_symbol("▌"),
            list,
            &mut self.hits_state,
        );
        let hit = self.hits_state.selected().and_then(|i| self.hits.get(i));
        let color = hit.map_or(Color::White, |h| color_of(&h.space));
        f.render_widget(self.record(hit.map(|h| h.id), color), preview);
    }

    // ---------- frame ----------

    pub fn draw(&mut self, f: &mut Frame) {
        let [top, main, bottom] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(8),
            Constraint::Length(1),
        ])
        .areas(f.area());
        f.render_widget(self.header(), top);
        match self.screen {
            Screen::Graph => self.draw_graph(f, main),
            Screen::Space => self.draw_space(f, main),
            Screen::Queue => self.draw_queue(f, main),
            Screen::Search => self.draw_search(f, main),
        }
        f.render_widget(self.footer(), bottom);
    }

    fn header(&self) -> Line<'_> {
        let records: i64 = self
            .graph
            .nodes
            .iter()
            .filter(|n| !n.hub)
            .map(Node::total)
            .sum();
        let tab = |label: &'static str, on: bool| {
            if on {
                Span::styled(
                    format!(" {label} "),
                    Style::new()
                        .fg(Color::Black)
                        .bg(Color::White)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                Span::styled(format!(" {label} "), Style::new().fg(DIM))
            }
        };
        Line::from(vec![
            Span::styled(" engram ", Style::new().add_modifier(Modifier::BOLD)),
            tab(
                "1 map",
                matches!(self.screen, Screen::Graph | Screen::Space),
            ),
            tab("2 queue", self.screen == Screen::Queue),
            tab("3 search", self.screen == Screen::Search),
            Span::styled(
                format!(
                    "   {records} records · {} spaces · {} to review",
                    self.graph.nodes.iter().filter(|n| !n.hub).count(),
                    self.queue.len()
                ),
                Style::new().fg(DIM),
            ),
        ])
    }

    fn footer(&self) -> Line<'_> {
        let keys = match self.screen {
            Screen::Graph => "←↑↓→ move · enter open · / search · tab queue · q quit",
            Screen::Space => "↑↓ move · p pin · a archive/restore · esc map · q quit",
            Screen::Queue => "↑↓ move · y accept · n reject · esc map · q quit",
            Screen::Search => "type, enter search · ↑↓ move · esc clear/back",
        };
        Line::from(vec![
            Span::styled(format!(" {keys}"), Style::new().fg(DIM)),
            Span::styled(
                format!("   {}", self.status),
                Style::new().fg(Color::Yellow),
            ),
        ])
    }
}

fn panel(title: &str) -> Block<'_> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(Color::Rgb(70, 74, 82)))
        .title(Span::styled(
            title,
            Style::new().add_modifier(Modifier::BOLD),
        ))
}

fn dim(c: Color) -> Color {
    match c {
        Color::Cyan => Color::Rgb(30, 90, 100),
        Color::Magenta => Color::Rgb(95, 45, 100),
        Color::Green => Color::Rgb(40, 90, 50),
        Color::Yellow => Color::Rgb(100, 90, 30),
        Color::Blue => Color::Rgb(40, 55, 110),
        Color::LightRed => Color::Rgb(110, 50, 50),
        _ => Color::Rgb(60, 60, 60),
    }
}

/// Titles imported from tiered stores start with a section marker; it is noise here.
fn tidy(title: &str) -> &str {
    title
        .trim_start_matches("ABSTRACT: ")
        .trim_start_matches("**")
}

fn bar(v: i64, max: i64, width: usize) -> String {
    let full = ((v as f64 / max as f64) * width as f64).round() as usize;
    format!(
        "{}{}",
        "█".repeat(full),
        "░".repeat(width - full.min(width))
    )
}

fn kind_letter(kind: &str) -> &'static str {
    match kind {
        "rule" => "r",
        "preference" => "p",
        "decision" => "d",
        "fact" => "f",
        "gotcha" => "g",
        "lesson" => "l",
        "state" => "s",
        "task" => "t",
        "person" => "@",
        "event" => "e",
        _ => "·",
    }
}

fn touches_global(e: &review::Entry) -> bool {
    e.op == "activate" || e.proposal["space"].as_str() == Some("global")
}

fn move_in(state: &mut ListState, len: usize, key: KeyCode) {
    if len == 0 {
        return;
    }
    let i = state.selected().unwrap_or(0);
    let next = match key {
        KeyCode::Down | KeyCode::Char('j') => (i + 1).min(len - 1),
        KeyCode::Up | KeyCode::Char('k') => i.saturating_sub(1),
        KeyCode::PageDown => (i + 15).min(len - 1),
        KeyCode::PageUp => i.saturating_sub(15),
        KeyCode::Home => 0,
        KeyCode::End => len - 1,
        _ => i,
    };
    state.select(Some(next));
}
