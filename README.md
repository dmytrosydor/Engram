# Engram

Local long-term memory for [Claude Code](https://code.claude.com). One Rust binary and one SQLite file. Search runs on your machine (full-text plus a local embedding model), and your work and personal projects never see each other's memory.

[![ci](https://github.com/dmytrosydor/Engram/actions/workflows/ci.yml/badge.svg)](https://github.com/dmytrosydor/Engram/actions/workflows/ci.yml)
![platforms](https://img.shields.io/badge/platform-macOS%20%7C%20Linux-lightgrey)
![license](https://img.shields.io/badge/license-MIT-blue)

- **A small core at every session start.** A `SessionStart` hook adds, in about a millisecond, a short summary of what matters in the current project (≤ 6,000 characters): rules and preferences, current state, recent decisions, what gets opened most. It does not dump all of memory into the context.
- **Search, get, write over MCP.** Claude looks things up on demand. Hybrid search combines two FTS5 indexes and vector search (bge-m3, multilingual), takes ~30 ms, and needs no API key.
- **Isolation enforced in code, not in a prompt.** Folders map to spaces, and spaces belong to domains such as `work` and `personal`. A session can only read and change its own domain plus a `global` space (details below).
- **It looks after itself.** Claude writes what is worth keeping during the session, and each new record comes back with older ones on the same topic, so outdated facts get replaced on the spot. Expired state is archived daily with no model calls and nothing for you to review.
- **A terminal UI.** Run `engram` with no arguments: a node map of your domains and spaces sized by how much each holds, a space's records, the review queue and search.

## Quickstart

You need macOS or Linux, [llama.cpp](https://github.com/ggml-org/llama.cpp) (for `llama-server`), the `claude` CLI and `curl`.

Pick your platform: `aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-unknown-linux-gnu` or `aarch64-unknown-linux-gnu`. The install line below uses Apple silicon. To build from source instead, use `cargo install --git https://github.com/dmytrosydor/Engram` (Rust 1.89+).

```sh
brew install llama.cpp                 # Linux: a llama.cpp release or your package manager (CUDA or CPU build)
mkdir -p ~/.local/bin && curl -L https://github.com/dmytrosydor/Engram/releases/latest/download/engram-aarch64-apple-darwin.tar.gz | tar xz -C ~/.local/bin engram
export PATH="$HOME/.local/bin:$PATH"   # add to your shell profile if it is not on PATH yet

engram model pull                      # 0.6 GB embedding model into ~/.engram/models, checksum verified
engram spaces --init                   # drafts ~/.engram/spaces.toml from the folders Claude Code has worked in
$EDITOR ~/.engram/spaces.toml          # name your domains (e.g. work / personal), drop what you don't need
engram spaces ~/.engram/spaces.toml
engram import auto-memory              # optional: bring in Claude Code's existing auto-memory
engram doctor                          # checks every piece
engram install                         # prints the plan; nothing changes yet
engram install --yes                   # hook + MCP server + hourly daily-pass job
```

Open a new Claude Code session in one of your projects. The first lines of context start with `engram memory · space …`.

## How it works

```
new session ──► SessionStart hook ──► core of the folder's space (prebuilt, ≤ 6k chars)

Claude ──MCP──► search / get / write ──► ~/.engram/engram.db
                                          SQLite · FTS5 trigram + word · sqlite-vec
                                          ▲
                     llama-server (bge-m3) ┘  on demand over a Unix socket, sleeps when idle

hourly job ──► engram daily --if-due ──► archive expired state, rebuild cores (no model)
```

- **Records** are short, typed and dated: `rule`, `preference`, `decision`, `fact`, `gotcha`, `lesson`, `state`, `task`, `person`, `event`. Titles are ≤ 120 characters and bodies ≤ 1,500.
- **Writing retired text brings it back.** If the same text exists as an archived or superseded record, `write` restores that record instead of reporting it as stored.
- **`write` refuses near-duplicates** (cosine ≥ 0.88) and lists them. Claude then picks `add`, `update:<id>` or `supersede:<id>`.
- **`write` flags what may be outdated.** A saved record comes back with up to three older ones on the same topic (cosine 0.58–0.88), so Claude can retire any that no longer hold with `supersede:<id>`. `global` records are only listed, since a session cannot change them.
- **State expires.** `state` and `task` records can carry `valid_until`, and the daily pass archives them once it passes. Old state also ranks lower in search.
- **The core is rebuilt** whenever a write touches it, and by the daily pass.

## Spaces, domains and what is guaranteed

`~/.engram/spaces.toml` maps folders to spaces. The longest matching folder prefix wins, and symlinks are resolved first.

```toml
[spaces]
"work/acme"      = { domain = "work" }
"work/acme-api"  = { domain = "work" }
"personal/notes" = { domain = "personal" }

[paths]
"~/code/acme"     = "work/acme"
"~/code/acme/api" = "work/acme-api"
"~/notes"         = "personal/notes"
```

Every domain automatically gets a `<domain>/_shared` space for knowledge its projects share. Two spaces are built in: `global`, visible to everyone, and `unsorted`, for folders without a rule.

The code guarantees the following:

- **What a session sees.** A session in space `S` of domain `D` sees `S`, `D/_shared` and `global`. With `scope: "all"` it sees every space of `D` plus `global`. A session in `unsorted` sees only `unsorted` and `global`.
- **`get`** opens what `search` can return: its own domain plus `global`. Anything else is "not found".
- **`write`** refuses to target a space the session cannot see. `update` and `supersede` work only on records the session can see, and never on `global` records, which no session can change.
- **Writes to `global`** are stored as unreviewed and reach other sessions' cores only after you accept them.
- **Duplicate detection** works within one space. Writing text that happens to exist in another domain neither fails nor reveals that the other copy exists.
- **Links** returned by `get` leave out records the session cannot see.
- **`engram daily --with-claude`** never puts two domains' records in the same model request. Everything it proposes stays in the record's own domain or waits for your approval.

What is **not** isolated:

- **The terminal UI and the CLI** show every domain: they are you, not a session.
- **The database file** is readable by your user account.
- **`engram daily --with-claude` sends text to Claude.** It runs `claude -p`, which sends the transcript excerpts and records it is judging to Anthropic, the same way your normal Claude Code sessions do. The default daily pass sends nothing.

## Terminal UI

```
 engram  1 map  2 queue  3 search    1045 records · 9 spaces · 0 to review
╭ memory map ──────────────────────────────────────────────────────────╮╭ selected ──────────────────────────╮
│                                                                      ││work/acme-web                       │
│                                                                      ││space in work                       │
│          shared 8  ⣀⡀                          ⣠⣤billing 90          ││                                    │
│                    ⠛⠃                          ⠿⠿⠃                   ││active     ██████████████ 420       │
│                     ⢇                          ⡸                     ││unreviewed ░░░░░░░░░░░░░░ 0         │
│                     ⠸⡀                         ⡇                     ││archived   ░░░░░░░░░░░░░░ 0         │
│                      ⡇                        ⢸                      ││                                    │
│                      ⢸                        ⡇                      ││last write 2026-10-29               │
│       blog 60 ⢀⣤⣄     ⡇                      ⢰⠁   ⣴⣿⣿⣷acme-web 420   ││sees       work/_shared, global     │
│               ⠘⠿⠟⡀    ⢸                      ⡎    ⢿⣿⣿⣿⠇              ││                                    │
│                  ⠈⠑⢄  ⠈⡆                    ⢰⠁  ⢀⠔⠁⠉⠉⠁               ││recent                              │
│                     ⠑⢄⡀⢱                    ⡜ ⡠⠊⠁                    ││· work/acme-web note 0              │
│               personal⠈⢀⣤⣀⣀⣀⣀⡀   ⣠⣄        ⣤work                     ││· work/acme-web note 1              │
│                       ⡠⠈⠛    global 12⠉⠉⠉⠉⠉⠛⠁⡀                       ││· work/acme-web note 2              │
│                    ⢀⡠⠊ ⡜                    ⢇⠈⠑⢄                     ││· work/acme-web note 3              │
│                  ⢀⠔⠁  ⢀⠇                    ⠸⡀  ⠑⢄⡀                  ││· work/acme-web note 4              │
│       game 25  ⣤⡄⠁    ⡸                      ⡇    ⢠⣶⣷acme-api 260    ││· work/acme-web note 5              │
│                ⠉⠁     ⡇                      ⢸    ⠈⠻⠿⠃               ││                                    │
│                      ⢸                        ⡇                      ││                                    │
│                      ⡇                        ⢸                      ││                                    │
│                     ⢸                         ⠈⡆                     ││                                    │
│                     ⡎                          ⢱                     ││                                    │
│         notes 140 ⢰⣶⣶                          ⢰⣦shared 30           ││                                    │
│                   ⠈⠛⠋                          ⠈⠉                    ││                                    │
│                                                                      ││                                    │
│                                                                      ││                                    │
╰──────────────────────────────────────────────────────────────────────╯╰────────────────────────────────────╯
 ←↑↓→ move · enter open · / search · tab queue · q quit
```

- **Map:** `global` in the middle, a hub per domain, its spaces around it. Node size follows the number of records; lines between spaces are confirmed links. Arrows move between nodes, Enter opens a space.
- **Space:** its records, newest first, with the full text. `p` pins, `a` archives or restores.
- **Queue:** usually empty. It holds writes headed for `global`, since every session would see them, plus whatever `engram daily --with-claude` proposes. `y` accepts, `n` rejects.
- **Search:** searches as a session in the selected space would.

## Daily pass

The scheduled job runs at most once a day and calls no model. It archives expired `state` and `task` records and rebuilds every core.

`engram daily --with-claude` is optional and manual. It also:

1. finds near-duplicate pairs within a domain and asks the model whether to merge them;
2. classifies unreviewed (imported) records as keep or archive;
3. reads Claude Code transcripts changed since the last run and proposes durable knowledge from them;
4. turns notes from `engram add "…"` and files dropped in `~/.engram/inbox/` into proposals.

Merges and new knowledge wait in the review queue. With the default limits a run makes up to about 15 `claude -p` calls on Sonnet. They count against your Claude Code plan, or are billed to your API key if you use one. Set `ENGRAM_DAILY_MODEL` to choose another model.

## Resources

- **Model:** 0.6 GB on disk (bge-m3, Q8 GGUF).
- **Memory:** about 820 MB while warm. After 10 idle minutes it drops to a ~180 MB process and wakes in ~0.4 s.
- **GPU:** Metal on Apple silicon and CUDA where llama.cpp has it; otherwise it runs on the CPU.
- **Search latency:** ~30 ms (p50) on an M-series Mac with ~3,000 records.
- **Hook:** about 1 ms.

## Configuration

| | |
|---|---|
| `~/.engram/spaces.toml` | spaces, domains, folder rules (`engram spaces <file>` loads it, `engram spaces` lists what is loaded) |
| `ENGRAM_HOME` | data folder (default `~/.engram`) |
| `ENGRAM_LLAMA_SERVER` | path to `llama-server` (default: `PATH`, then `/opt/homebrew/bin`, `/usr/local/bin`, `/usr/bin`, `~/.local/bin`) |
| `ENGRAM_EMBED_SOCK` | Unix socket for llama-server (default `~/.engram/embed.sock`) |
| `ENGRAM_DAILY_MODEL` | model for `engram daily --with-claude` (default `sonnet`) |

## Importing

- `engram import auto-memory` reads `~/.claude/projects/*/memory/*.md` and places each project by your folder rules. Records from unmapped projects wait in `unsorted` as unreviewed. Re-running only adds what is new.
- `engram import memora [--space S] [--manifest file.csv]` imports a Memora database read-only. Without a manifest every record lands in `--space` (default `unsorted`) as unreviewed, and the daily pass sorts it.

After an import, `engram reindex` computes the vectors. That takes about 5 minutes for 3,000 records.

## Commands

| | |
|---|---|
| `engram doctor` | check database, model, llama-server, claude, hook, MCP and schedule |
| `engram where [dir]` | which space a folder maps to and what it sees |
| `engram core [dir]` / `--all` | the core a session there gets / sizes of all cores |
| `engram search "q" [--dir d] [--scope project\|all\|archive]` | search as Claude would from a folder |
| `engram get <id>…` | records in full |
| `engram add "note"` | drop a note for the next `engram daily --with-claude` |
| `engram review list \| accept <id> \| reject <id>` | the review queue |
| `engram move --to <space> <id>…` | move records to another space |
| `engram daily [--with-claude]` | run the daily pass now, optionally with the model |
| `engram` | the terminal UI (also `engram tui`) |
| `engram eval extract && engram eval run` | search quality on your own past sessions (Memora history) |
| `engram uninstall --yes` | remove hook, MCP server and schedule; data stays |

## Uninstall

```sh
engram uninstall --yes    # removes the hook, the MCP server and the scheduled job
rm -rf ~/.engram          # removes the data and the model, if you want that too
```

`engram install` backs up `~/.claude/settings.json` before editing it, as `settings.json.engram-<time>`.

## Troubleshooting

- **No core at session start.** Run `echo '{"cwd":"'$PWD'"}' | engram hook session-start`. If it prints nothing, the folder's space has no records yet, or the folder has no rule (`engram where`).
- **The MCP server is not connected.** Run `claude mcp get engram`. A local- or project-scope server with the same name overrides the user-scope one.
- **Search says "words only".** The model or `llama-server` is missing; see `engram doctor`.
- **`command not found: engram`.** Put `~/.local/bin` (or `~/.cargo/bin` if you built from source) on your `PATH`.
- **Wrong space.** Run `engram where`. Rules are folder prefixes after symlinks are resolved; reload `spaces.toml` after editing it.

## FAQ

**Does it replace Claude Code's auto-memory?** It can. Import auto-memory once, then turn auto-memory off (`autoMemoryEnabled: false`) so the same knowledge does not load twice.

**Windows?** Not supported natively. Under WSL it behaves like Linux.

**Other MCP clients?** `engram mcp` is a standard stdio MCP server. The space comes from the folder the client starts it in, or from `CLAUDE_PROJECT_DIR`.

## Development

```sh
cargo test                        # no model needed
cargo test -- --include-ignored   # also the model-backed tests (model + llama-server required)
cargo clippy --all-targets
```

## License

MIT, see [LICENSE](LICENSE). The embedding model [BAAI/bge-m3](https://huggingface.co/BAAI/bge-m3) (MIT) is downloaded separately.
