# Engram

Local long-term memory for Claude Code. One Rust binary, one SQLite file (FTS5 + `sqlite-vec`) and a local bge-m3 embedding model served by llama.cpp on Metal.

- **A core at session start.** A `SessionStart` hook adds, in about 1 ms, a short core for the space of the current folder (≤ 6,000 characters): rules, current state, recent decisions and what gets opened most.
- **Three MCP tools:** `search` (by words and by meaning, scoped to the space), `get`, and `write` (refuses near-duplicates until you pick a mode).
- **Spaces.** `work/…` and `personal/…` are resolved from the folder; work never sees personal records and vice versa. `global` is visible everywhere.
- **Daily pass.** Once a day (on AC power) expired state is archived, near-duplicates and new knowledge from transcripts go to a review queue, and unreviewed imports are classified. The model is an isolated `claude -p` with no tools.
- **Web UI** on `127.0.0.1:7777`: what Claude gets, the queue, a feed, a map, the recall log and health.

## Install

```sh
brew install rust llama.cpp
cargo install --path .
mkdir -p ~/.engram/models
curl -L -o ~/.engram/models/bge-m3-Q8_0.gguf \
  https://huggingface.co/gpustack/bge-m3-GGUF/resolve/main/bge-m3-Q8_0.gguf
```

Spaces and folder → space rules live in `~/.engram/spaces.toml`:

```toml
[spaces]
"work/acme" = { domain = "work" }
"personal/notes" = { domain = "personal" }

[paths]
"~/code/acme" = "work/acme"
"~/notes" = "personal/notes"
```

```sh
engram spaces ~/.engram/spaces.toml
engram install          # shows the plan
engram install --yes    # SessionStart hook, MCP server, launchd agent for the daily pass
```

`engram uninstall --yes` removes all of that; the data in `~/.engram` stays.

## Import (optional)

From Memora and Claude Code auto-memory; the sources are only read. Classification comes from CSV manifests (one row per record); records without a row go to `personal/_inbox` as unreviewed.

```sh
engram import memora --manifest memora-manifest.csv
engram import auto-memory --inventory auto-memory-inventory.csv
engram reindex
```

## Day to day

```sh
engram ui                      # http://127.0.0.1:7777
engram search "query" --dir ~/code/acme
engram add "a note for the next daily pass"
engram review list | accept <id> | reject <id>
engram daily                   # run now instead of waiting for launchd
engram eval extract && engram eval run   # search quality on your own history
```

The data folder is `~/.engram` (or `ENGRAM_HOME`); the model and its socket always live in `~/.engram`.

## Development

```sh
cargo test                     # no model needed
cargo test -- --include-ignored   # with the model in ~/.engram/models
```
