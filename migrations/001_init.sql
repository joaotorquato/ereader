-- Esquema inicial. Aplicado no boot por db::migrate() (idempotente via schema_migrations).

CREATE TABLE IF NOT EXISTS schema_migrations (
  version    INTEGER PRIMARY KEY,
  applied_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS books (
  id            INTEGER PRIMARY KEY,
  title         TEXT NOT NULL,
  lang          TEXT NOT NULL DEFAULT '',     -- BCP-47 do livro ('en', 'pt-BR', '' se desconhecido)
  kind          TEXT NOT NULL,                -- 'pdf' | 'epub'
  file_path     TEXT NOT NULL,                -- caminho relativo ao data-dir do original
  section_count INTEGER NOT NULL DEFAULT 0,
  -- posição de leitura (lembrada por livro)
  pos_section   INTEGER NOT NULL DEFAULT 0,
  pos_paragraph INTEGER NOT NULL DEFAULT 0,
  created_at    TEXT NOT NULL DEFAULT (datetime('now')),
  opened_at     TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS sections (
  id       INTEGER PRIMARY KEY,
  book_id  INTEGER NOT NULL REFERENCES books(id) ON DELETE CASCADE,
  idx      INTEGER NOT NULL,                  -- 0-based, ordem de leitura
  title    TEXT NOT NULL,
  UNIQUE (book_id, idx)
);

CREATE TABLE IF NOT EXISTS paragraphs (
  id         INTEGER PRIMARY KEY,
  section_id INTEGER NOT NULL REFERENCES sections(id) ON DELETE CASCADE,
  idx        INTEGER NOT NULL,                -- 0-based dentro da seção
  kind       TEXT NOT NULL DEFAULT 'p',       -- 'h1'..'h6' | 'p' | 'li' | 'blockquote'
  text       TEXT NOT NULL,
  UNIQUE (section_id, idx)
);

-- Cache de áudio. A chave é sha256(texto normalizado + voz + velocidade),
-- então o mesmo trecho em dois livros compartilha o clip.
CREATE TABLE IF NOT EXISTS clips (
  key          TEXT PRIMARY KEY,
  text         TEXT NOT NULL,
  voice        TEXT NOT NULL,
  speed        REAL NOT NULL,
  lang         TEXT NOT NULL,                 -- código do espeak: 'en-us' | 'pt-br'
  path         TEXT NOT NULL,                 -- relativo ao data-dir: audio/ab/abcdef....wav
  duration_ms  INTEGER NOT NULL,
  word_timings TEXT NOT NULL,                 -- JSON: [{"o":offset_utf16,"l":len_utf16,"s":start_ms,"e":end_ms}]
  aligned      INTEGER NOT NULL DEFAULT 0,    -- 1 se veio de durations do modelo, 0 se proporcional
  created_at   TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_paragraphs_section ON paragraphs(section_id, idx);
CREATE INDEX IF NOT EXISTS idx_sections_book ON sections(book_id, idx);
