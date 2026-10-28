# Engram

Локальна довготривала пам'ять для Claude Code. Один бінарник на Rust, один файл SQLite (FTS5 + `sqlite-vec`) і локальна модель векторів bge-m3 через llama.cpp на Metal.

- **Ядро на старті сесії.** Хук `SessionStart` за ~1 мс додає коротке ядро простору поточної папки (≤ 6 000 символів): правила, поточний стан, свіжі рішення, те, що найчастіше відкривається.
- **Три інструменти MCP:** `search` (за словами + за змістом, з урахуванням простору), `get`, `write` (не пише майже-дублі, поки не вибрано режим).
- **Простори.** `work/…` і `personal/…` визначаються за папкою; робота не бачить особистого, і навпаки. `global` видно всюди.
- **Щоденний розбір.** Раз на день (на живленні) прострочений стан іде в архів, майже-дублі й нові знання з транскриптів потрапляють у чергу, неперевірений імпорт класифікується. Модель — ізольований `claude -p` без інструментів.
- **Веб-інтерфейс** на `127.0.0.1:7777`: що отримає Claude, черга, стрічка, карта, журнал видач, здоров'я.

## Встановлення

```sh
brew install rust llama.cpp
cargo install --path .
mkdir -p ~/.engram/models
curl -L -o ~/.engram/models/bge-m3-Q8_0.gguf \
  https://huggingface.co/gpustack/bge-m3-GGUF/resolve/main/bge-m3-Q8_0.gguf
```

Простори й правила «папка → простір» — у `~/.engram/spaces.toml`:

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
engram install          # показує план
engram install --yes    # хук SessionStart, MCP-сервер, launchd-агент щоденного розбору
```

`engram uninstall --yes` прибирає все це; дані в `~/.engram` лишаються.

## Імпорт (необов'язково)

З Memora і auto-memory Claude Code; джерела лише читаються. Класифікація — CSV-маніфести (один рядок на запис); записи без рядка йдуть в `personal/_inbox` як неперевірені.

```sh
engram import memora --manifest memora-manifest.csv
engram import auto-memory --inventory auto-memory-inventory.csv
engram reindex
```

## Щодня

```sh
engram ui                      # http://127.0.0.1:7777
engram search "запит" --dir ~/code/acme
engram add "нотатка для наступного розбору"
engram review list | accept <id> | reject <id>
engram daily                   # вручну, не чекаючи launchd
engram eval extract && engram eval run   # якість пошуку на власній історії
```

Тека даних — `~/.engram` (або `ENGRAM_HOME`); модель і її сокет завжди в `~/.engram`.

## Розробка

```sh
cargo test                     # без моделі
cargo test -- --include-ignored   # з моделлю в ~/.engram/models
```
