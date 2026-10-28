-- Engram schema. Every statement is idempotent: `engram init` can be re-run safely.

PRAGMA journal_mode = WAL;
PRAGMA user_version = 1;

CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
-- Changing the embedding model means re-embedding everything, so it is recorded here.
INSERT OR IGNORE INTO meta (key, value) VALUES
  ('embed_model', 'BAAI/bge-m3'),
  ('embed_dim', '1024');

-- Spaces: 'global', 'work/acme', 'personal/notes', ...
CREATE TABLE IF NOT EXISTS space (
  id TEXT PRIMARY KEY,
  domain TEXT NOT NULL CHECK (domain IN ('global', 'work', 'personal')),
  core TEXT,                           -- prebuilt session-start core, rebuilt by the daily pass
  core_tokens INTEGER,
  core_built_at TEXT
);

-- Session folder -> space; the longest matching prefix wins.
CREATE TABLE IF NOT EXISTS path_rule (
  prefix TEXT PRIMARY KEY,
  space_id TEXT NOT NULL REFERENCES space (id)
);

CREATE TABLE IF NOT EXISTS item (
  id INTEGER PRIMARY KEY,
  space_id TEXT NOT NULL REFERENCES space (id),
  kind TEXT NOT NULL CHECK (kind IN
    ('rule', 'preference', 'decision', 'fact', 'gotcha', 'lesson', 'state', 'task', 'person', 'event')),
  title TEXT NOT NULL,                 -- <=120 chars, enforced in code
  body TEXT NOT NULL,                  -- <=1500 chars, enforced in code
  status TEXT NOT NULL DEFAULT 'active'
    CHECK (status IN ('proposed', 'active', 'superseded', 'archived')),
  pinned INTEGER NOT NULL DEFAULT 0,   -- 1 = goes into the space core
  valid_until TEXT,                    -- state/task: archived after this date
  superseded_by INTEGER REFERENCES item (id),
  source TEXT,                         -- 'session:<uuid>', 'pr:<repo>#<n>', 'chat:<...>', 'note'
  created_at TEXT NOT NULL,
  updated_at TEXT,
  hits INTEGER NOT NULL DEFAULT 0,     -- usefulness signal
  last_hit TEXT,
  content_hash TEXT UNIQUE,            -- exact duplicates are rejected
  legacy_id TEXT                       -- 'memora:<id>' / 'auto:<file>'
);
CREATE INDEX IF NOT EXISTS item_space_status ON item (space_id, status, kind);
CREATE UNIQUE INDEX IF NOT EXISTS item_legacy ON item (legacy_id);

-- Closed tag vocabulary.
CREATE TABLE IF NOT EXISTS tag_vocab (tag TEXT PRIMARY KEY, note TEXT);
CREATE TABLE IF NOT EXISTS tag (
  item_id INTEGER REFERENCES item (id) ON DELETE CASCADE,
  tag TEXT REFERENCES tag_vocab (tag),
  PRIMARY KEY (item_id, tag)
);

CREATE TABLE IF NOT EXISTS edge (
  src INTEGER REFERENCES item (id) ON DELETE CASCADE,
  dst INTEGER REFERENCES item (id) ON DELETE CASCADE,
  type TEXT NOT NULL CHECK (type IN
    ('supersedes', 'contradicts', 'depends_on', 'part_of', 'about', 'derived_from')),
  confirmed INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (src, dst, type)
);
CREATE INDEX IF NOT EXISTS edge_dst ON edge (dst);

-- Keyword search over items. trigram handles word forms and code identifiers;
-- unicode61 covers words shorter than 3 characters, which trigram cannot see.
CREATE VIRTUAL TABLE IF NOT EXISTS item_fts USING fts5 (
  title, body, content = 'item', content_rowid = 'id', tokenize = 'trigram'
);
CREATE VIRTUAL TABLE IF NOT EXISTS item_fts_word USING fts5 (
  title, body, content = 'item', content_rowid = 'id', tokenize = 'unicode61 remove_diacritics 2'
);
CREATE VIRTUAL TABLE IF NOT EXISTS item_vec USING vec0 (item_id INTEGER PRIMARY KEY, emb float[1024]);

CREATE TRIGGER IF NOT EXISTS item_ai AFTER INSERT ON item BEGIN
  INSERT INTO item_fts (rowid, title, body) VALUES (new.id, new.title, new.body);
  INSERT INTO item_fts_word (rowid, title, body) VALUES (new.id, new.title, new.body);
END;
CREATE TRIGGER IF NOT EXISTS item_ad AFTER DELETE ON item BEGIN
  INSERT INTO item_fts (item_fts, rowid, title, body) VALUES ('delete', old.id, old.title, old.body);
  INSERT INTO item_fts_word (item_fts_word, rowid, title, body) VALUES ('delete', old.id, old.title, old.body);
  DELETE FROM item_vec WHERE item_id = old.id;
END;
CREATE TRIGGER IF NOT EXISTS item_au AFTER UPDATE OF title, body ON item BEGIN
  INSERT INTO item_fts (item_fts, rowid, title, body) VALUES ('delete', old.id, old.title, old.body);
  INSERT INTO item_fts_word (item_fts_word, rowid, title, body) VALUES ('delete', old.id, old.title, old.body);
  INSERT INTO item_fts (rowid, title, body) VALUES (new.id, new.title, new.body);
  INSERT INTO item_fts_word (rowid, title, body) VALUES (new.id, new.title, new.body);
END;

-- Documents are kept apart from facts.
CREATE TABLE IF NOT EXISTS doc (
  id INTEGER PRIMARY KEY,
  space_id TEXT REFERENCES space (id),
  title TEXT,
  uri TEXT,
  sha TEXT,
  added_at TEXT
);
CREATE TABLE IF NOT EXISTS chunk (
  id INTEGER PRIMARY KEY,
  doc_id INTEGER REFERENCES doc (id) ON DELETE CASCADE,
  ord INTEGER,
  text TEXT
);
CREATE UNIQUE INDEX IF NOT EXISTS doc_uri ON doc (uri);
CREATE INDEX IF NOT EXISTS chunk_doc ON chunk (doc_id, ord);

CREATE VIRTUAL TABLE IF NOT EXISTS chunk_fts USING fts5 (
  text, content = 'chunk', content_rowid = 'id', tokenize = 'trigram'
);
CREATE VIRTUAL TABLE IF NOT EXISTS chunk_fts_word USING fts5 (
  text, content = 'chunk', content_rowid = 'id', tokenize = 'unicode61 remove_diacritics 2'
);
CREATE VIRTUAL TABLE IF NOT EXISTS chunk_vec USING vec0 (chunk_id INTEGER PRIMARY KEY, emb float[1024]);

CREATE TRIGGER IF NOT EXISTS chunk_ai AFTER INSERT ON chunk BEGIN
  INSERT INTO chunk_fts (rowid, text) VALUES (new.id, new.text);
  INSERT INTO chunk_fts_word (rowid, text) VALUES (new.id, new.text);
END;
CREATE TRIGGER IF NOT EXISTS chunk_ad AFTER DELETE ON chunk BEGIN
  INSERT INTO chunk_fts (chunk_fts, rowid, text) VALUES ('delete', old.id, old.text);
  INSERT INTO chunk_fts_word (chunk_fts_word, rowid, text) VALUES ('delete', old.id, old.text);
  DELETE FROM chunk_vec WHERE chunk_id = old.id;
END;
CREATE TRIGGER IF NOT EXISTS chunk_au AFTER UPDATE OF text ON chunk BEGIN
  INSERT INTO chunk_fts (chunk_fts, rowid, text) VALUES ('delete', old.id, old.text);
  INSERT INTO chunk_fts_word (chunk_fts_word, rowid, text) VALUES ('delete', old.id, old.text);
  INSERT INTO chunk_fts (rowid, text) VALUES (new.id, new.text);
  INSERT INTO chunk_fts_word (rowid, text) VALUES (new.id, new.text);
END;

-- Raw captures (notes, chats) waiting for the daily pass.
CREATE TABLE IF NOT EXISTS inbox (
  id INTEGER PRIMARY KEY,
  space_id TEXT,
  source TEXT,
  raw TEXT,
  created_at TEXT,
  processed_at TEXT
);

-- Proposals (merge, supersede, new items) awaiting a yes/no.
CREATE TABLE IF NOT EXISTS review (
  id INTEGER PRIMARY KEY,
  op TEXT,
  item_ids TEXT,
  proposal TEXT,
  reason TEXT,
  status TEXT DEFAULT 'open',
  created_at TEXT,
  decided_at TEXT
);

-- Every change keeps before/after, so anything can be rolled back.
CREATE TABLE IF NOT EXISTS log (
  id INTEGER PRIMARY KEY,
  at TEXT,
  actor TEXT,
  op TEXT,
  item_id INTEGER,
  before TEXT,
  after TEXT
);

-- What search returned and what was opened afterwards.
CREATE TABLE IF NOT EXISTS recall (
  id INTEGER PRIMARY KEY,
  at TEXT,
  session TEXT,
  space_id TEXT,
  query TEXT,
  item_ids TEXT,
  opened TEXT
);
