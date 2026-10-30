# Contributing

Issues and pull requests are welcome. For a bigger change, open an issue first so we can agree on the approach.

## Setup

You need macOS or Linux and [Rust](https://rustup.rs) 1.89+. Windows is not supported natively; under WSL it behaves like Linux.

The [Quickstart](README.md#quickstart) covers `llama-server` and the embedding model. You only need them for the model-backed tests.

## Layout

- `src/` — the library and the `engram` binary. `src/schema.sql` is the database schema.
- `tests/` — integration tests.

## Before you open a pull request

CI runs these on macOS and Linux:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

The model-backed tests are `#[ignore]`d. Run them when you change search or embeddings:

```sh
cargo test -- --include-ignored   # model + llama-server required
```

## Isolation

The guarantees in [Spaces, domains and what is guaranteed](README.md#spaces-domains-and-what-is-guaranteed) are the core promise of the project. A change that touches what a session can see or write should come with a test for it.

## Style

Match the surrounding code. Commits follow [Conventional Commits](https://www.conventionalcommits.org).
