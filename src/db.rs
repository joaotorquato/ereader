//! Acesso ao SQLite via rusqlite, sem ORM.
//!
//! Ownership: uma única `Connection` atrás de `std::sync::Mutex`, compartilhada por
//! `Arc` no estado do axum. SQLite serializa escritas de qualquer jeito, e todas as
//! queries aqui são de microssegundos, então segurar um Mutex síncrono dentro de um
//! handler async é aceitável (não há await com o lock preso). Se um dia isso virar
//! gargalo, o caminho é `spawn_blocking` + pool, não async-sqlite.

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use std::path::Path;
use std::sync::Mutex;

/// Migrations embutidas no binário, em ordem. Adicione `(2, include_str!(...))` etc.
const MIGRATIONS: &[(i64, &str)] = &[(1, include_str!("../migrations/001_init.sql"))];

pub struct Db {
    conn: Mutex<Connection>,
}

#[derive(Debug, Serialize, Clone)]
pub struct Book {
    pub id: i64,
    pub title: String,
    pub lang: String,
    pub kind: String,
    pub section_count: i64,
    pub pos_section: i64,
    pub pos_paragraph: i64,
    pub created_at: String,
    pub opened_at: String,
}

#[derive(Debug, Serialize, Clone)]
pub struct Section {
    pub id: i64,
    pub idx: i64,
    pub title: String,
}

#[derive(Debug, Serialize, Clone)]
pub struct Paragraph {
    pub id: i64,
    pub section_id: i64,
    pub idx: i64,
    pub kind: String,
    pub text: String,
}

#[derive(Debug, Serialize, Clone)]
pub struct Clip {
    pub key: String,
    pub path: String,
    pub duration_ms: i64,
    /// JSON já serializado (o handler devolve como `serde_json::value::RawValue`-like
    /// via `serde_json::from_str` — simples e evita parsear duas vezes).
    pub word_timings: String,
    pub aligned: bool,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path).with_context(|| format!("abrindo {}", path.display()))?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;
             PRAGMA busy_timeout = 5000;",
        )?;
        let db = Db {
            conn: Mutex::new(conn),
        };
        db.migrate()?;
        Ok(db)
    }

    fn migrate(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_migrations (
               version INTEGER PRIMARY KEY,
               applied_at TEXT NOT NULL DEFAULT (datetime('now')));",
        )?;
        for (version, sql) in MIGRATIONS {
            let applied: Option<i64> = conn
                .query_row(
                    "SELECT version FROM schema_migrations WHERE version = ?1",
                    params![version],
                    |r| r.get(0),
                )
                .optional()?;
            if applied.is_some() {
                continue;
            }
            tracing::info!(version, "aplicando migration");
            conn.execute_batch(sql)?;
            conn.execute(
                "INSERT INTO schema_migrations (version) VALUES (?1)",
                params![version],
            )?;
        }
        Ok(())
    }

    // ------------------------------------------------------------------ books

    /// Insere livro + seções + parágrafos numa transação só.
    pub fn insert_book(
        &self,
        title: &str,
        lang: &str,
        kind: &str,
        file_path: &str,
        sections: &[crate::extract::Section],
    ) -> Result<i64> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO books (title, lang, kind, file_path, section_count) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![title, lang, kind, file_path, sections.len() as i64],
        )?;
        let book_id = tx.last_insert_rowid();
        {
            let mut ins_sec =
                tx.prepare("INSERT INTO sections (book_id, idx, title) VALUES (?1, ?2, ?3)")?;
            let mut ins_par = tx.prepare(
                "INSERT INTO paragraphs (section_id, idx, kind, text) VALUES (?1, ?2, ?3, ?4)",
            )?;
            for (si, s) in sections.iter().enumerate() {
                ins_sec.execute(params![book_id, si as i64, s.title])?;
                let section_id = tx.last_insert_rowid();
                for (pi, p) in s.paragraphs.iter().enumerate() {
                    ins_par.execute(params![section_id, pi as i64, p.kind.as_str(), p.text])?;
                }
            }
        }
        tx.commit()?;
        Ok(book_id)
    }

    pub fn list_books(&self) -> Result<Vec<Book>> {
        let conn = self.conn.lock().unwrap();
        let mut st = conn.prepare(
            "SELECT id, title, lang, kind, section_count, pos_section, pos_paragraph, created_at, opened_at
             FROM books ORDER BY opened_at DESC",
        )?;
        let rows = st.query_map([], row_to_book)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn get_book(&self, id: i64) -> Result<Option<Book>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                "SELECT id, title, lang, kind, section_count, pos_section, pos_paragraph, created_at, opened_at
                 FROM books WHERE id = ?1",
                params![id],
                row_to_book,
            )
            .optional()?)
    }

    /// Remove o livro (cascade em seções/parágrafos) e devolve o caminho do arquivo original.
    pub fn delete_book(&self, id: i64) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        let path: Option<String> = conn
            .query_row(
                "SELECT file_path FROM books WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .optional()?;
        if path.is_some() {
            conn.execute("DELETE FROM books WHERE id = ?1", params![id])?;
        }
        Ok(path)
    }

    pub fn touch_book(&self, id: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE books SET opened_at = datetime('now') WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    }

    pub fn set_position(&self, id: i64, section: i64, paragraph: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE books SET pos_section = ?2, pos_paragraph = ?3, opened_at = datetime('now') WHERE id = ?1",
            params![id, section, paragraph],
        )?;
        Ok(())
    }

    // --------------------------------------------------------------- sections

    pub fn list_sections(&self, book_id: i64) -> Result<Vec<Section>> {
        let conn = self.conn.lock().unwrap();
        let mut st =
            conn.prepare("SELECT id, idx, title FROM sections WHERE book_id = ?1 ORDER BY idx")?;
        let rows = st.query_map(params![book_id], |r| {
            Ok(Section {
                id: r.get(0)?,
                idx: r.get(1)?,
                title: r.get(2)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn get_section(&self, book_id: i64, idx: i64) -> Result<Option<Section>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                "SELECT id, idx, title FROM sections WHERE book_id = ?1 AND idx = ?2",
                params![book_id, idx],
                |r| {
                    Ok(Section {
                        id: r.get(0)?,
                        idx: r.get(1)?,
                        title: r.get(2)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn list_paragraphs(&self, section_id: i64) -> Result<Vec<Paragraph>> {
        let conn = self.conn.lock().unwrap();
        let mut st = conn.prepare(
            "SELECT id, section_id, idx, kind, text FROM paragraphs WHERE section_id = ?1 ORDER BY idx",
        )?;
        let rows = st.query_map(params![section_id], row_to_paragraph)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn get_paragraph(&self, id: i64) -> Result<Option<Paragraph>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                "SELECT id, section_id, idx, kind, text FROM paragraphs WHERE id = ?1",
                params![id],
                row_to_paragraph,
            )
            .optional()?)
    }

    /// Os próximos `n` parágrafos depois de `after` na mesma seção (para prefetch).
    pub fn following_paragraphs(
        &self,
        section_id: i64,
        after_idx: i64,
        n: i64,
    ) -> Result<Vec<Paragraph>> {
        let conn = self.conn.lock().unwrap();
        let mut st = conn.prepare(
            "SELECT id, section_id, idx, kind, text FROM paragraphs
             WHERE section_id = ?1 AND idx > ?2 ORDER BY idx LIMIT ?3",
        )?;
        let rows = st.query_map(params![section_id, after_idx, n], row_to_paragraph)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Idioma do livro dono de um parágrafo (para escolher voz/fonemizador).
    pub fn paragraph_lang(&self, paragraph_id: i64) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                "SELECT b.lang FROM paragraphs p
                 JOIN sections s ON s.id = p.section_id
                 JOIN books b ON b.id = s.book_id
                 WHERE p.id = ?1",
                params![paragraph_id],
                |r| r.get(0),
            )
            .optional()?)
    }

    // ------------------------------------------------------------------ clips

    pub fn get_clip(&self, key: &str) -> Result<Option<Clip>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                "SELECT key, path, duration_ms, word_timings, aligned FROM clips WHERE key = ?1",
                params![key],
                |r| {
                    Ok(Clip {
                        key: r.get(0)?,
                        path: r.get(1)?,
                        duration_ms: r.get(2)?,
                        word_timings: r.get(3)?,
                        aligned: r.get::<_, i64>(4)? != 0,
                    })
                },
            )
            .optional()?)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn insert_clip(
        &self,
        key: &str,
        text: &str,
        voice: &str,
        speed: f32,
        lang: &str,
        path: &str,
        duration_ms: i64,
        word_timings_json: &str,
        aligned: bool,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO clips (key, text, voice, speed, lang, path, duration_ms, word_timings, aligned)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![key, text, voice, speed as f64, lang, path, duration_ms, word_timings_json, aligned as i64],
        )?;
        Ok(())
    }
}

fn row_to_book(r: &rusqlite::Row) -> rusqlite::Result<Book> {
    Ok(Book {
        id: r.get(0)?,
        title: r.get(1)?,
        lang: r.get(2)?,
        kind: r.get(3)?,
        section_count: r.get(4)?,
        pos_section: r.get(5)?,
        pos_paragraph: r.get(6)?,
        created_at: r.get(7)?,
        opened_at: r.get(8)?,
    })
}

fn row_to_paragraph(r: &rusqlite::Row) -> rusqlite::Result<Paragraph> {
    Ok(Paragraph {
        id: r.get(0)?,
        section_id: r.get(1)?,
        idx: r.get(2)?,
        kind: r.get(3)?,
        text: r.get(4)?,
    })
}
