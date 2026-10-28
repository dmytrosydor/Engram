# Engram

Локальна довготривала пам'ять для Claude Code: один бінарник на Rust і один файл SQLite з FTS5 і `sqlite-vec`.

## Збирання

Потрібен Rust (`brew install rust`). SQLite і `sqlite-vec` вбудовані в бінарник.

```sh
cargo build --release
./target/release/engram init   # створює ~/.engram/engram.db, повторний запуск безпечний
./target/release/engram info   # версії SQLite і sqlite-vec, список таблиць
```

Тека даних — `~/.engram`, або змінна `ENGRAM_HOME`.

## Тести

```sh
cargo test
```
