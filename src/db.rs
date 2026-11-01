use crate::config::{db_path, ensure_config_dir};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;

/// Open a database at an explicit path (used by tests), creating the schema.
pub fn open_db_at(path: &str) -> rusqlite::Result<Connection> {
    let conn = if path == ":memory:" {
        Connection::open_in_memory()?
    } else {
        Connection::open(Path::new(path))?
    };
    let _ = conn.pragma_update(None, "journal_mode", "WAL");
    let _ = conn.pragma_update(None, "synchronous", "NORMAL");
    migrate(&conn)?;
    Ok(conn)
}

/// Open the index database, creating the schema if needed.
pub fn open_db() -> rusqlite::Result<Connection> {
    ensure_config_dir();
    let p = db_path();
    open_db_at(&p.to_string_lossy())
}

fn migrate(db: &Connection) -> rusqlite::Result<()> {
    db.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS files (
          path          TEXT PRIMARY KEY,
          mtime_ms      INTEGER NOT NULL,
          size          INTEGER NOT NULL,
          hash          TEXT NOT NULL,
          title         TEXT,
          frontmatter   TEXT,
          indexed_at    INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS blocks (
          id            INTEGER PRIMARY KEY AUTOINCREMENT,
          path          TEXT NOT NULL,
          heading_path  TEXT NOT NULL,
          start_line    INTEGER NOT NULL,
          end_line      INTEGER NOT NULL,
          text          TEXT NOT NULL,
          embedding     BLOB,
          FOREIGN KEY (path) REFERENCES files(path) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS idx_blocks_path ON blocks(path);

        CREATE TABLE IF NOT EXISTS embedding_cache (
          text_hash     TEXT PRIMARY KEY,
          embedding     BLOB NOT NULL,
          updated_at    INTEGER NOT NULL
        );

        CREATE VIRTUAL TABLE IF NOT EXISTS blocks_fts USING fts5(
          text,
          heading_path,
          content='blocks',
          content_rowid='id',
          tokenize='porter unicode61 remove_diacritics 2'
        );

        CREATE TRIGGER IF NOT EXISTS blocks_ai AFTER INSERT ON blocks BEGIN
          INSERT INTO blocks_fts(rowid, text, heading_path)
          VALUES (new.id, new.text, new.heading_path);
        END;
        CREATE TRIGGER IF NOT EXISTS blocks_ad AFTER DELETE ON blocks BEGIN
          INSERT INTO blocks_fts(blocks_fts, rowid, text, heading_path)
          VALUES ('delete', old.id, old.text, old.heading_path);
        END;

        CREATE TABLE IF NOT EXISTS links (
          src_path      TEXT NOT NULL,
          target_raw    TEXT NOT NULL,
          target_path   TEXT,
          heading       TEXT,
          alias         TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_links_src ON links(src_path);
        CREATE INDEX IF NOT EXISTS idx_links_target ON links(target_path);

        CREATE TABLE IF NOT EXISTS meta (
          key           TEXT PRIMARY KEY,
          value         TEXT
        );

        CREATE TABLE IF NOT EXISTS sync_state (
          path          TEXT PRIMARY KEY,
          remote_hash   TEXT NOT NULL,
          remote_mtime  INTEGER NOT NULL,
          synced_at     INTEGER NOT NULL,
          base_content  TEXT NOT NULL DEFAULT ''
        );
        "#,
    )?;

    if get_meta(db, "index_engine").as_deref() != Some("multilingual_v3") {
        let _ = db.execute_batch(
            r#"
            DROP TRIGGER IF EXISTS blocks_ai;
            DROP TRIGGER IF EXISTS blocks_ad;
            DROP TABLE IF EXISTS blocks_fts;
            CREATE VIRTUAL TABLE blocks_fts USING fts5(
              text,
              heading_path,
              content='blocks',
              content_rowid='id',
              tokenize='porter unicode61 remove_diacritics 2'
            );
            CREATE TRIGGER blocks_ai AFTER INSERT ON blocks BEGIN
              INSERT INTO blocks_fts(rowid, text, heading_path)
              VALUES (new.id, new.text, new.heading_path);
            END;
            CREATE TRIGGER blocks_ad AFTER DELETE ON blocks BEGIN
              INSERT INTO blocks_fts(blocks_fts, rowid, text, heading_path)
              VALUES ('delete', old.id, old.text, old.heading_path);
            END;
            INSERT INTO blocks_fts(blocks_fts) VALUES('rebuild');
            DELETE FROM embedding_cache;
            UPDATE blocks SET embedding = NULL;
            "#,
        );
        set_meta(db, "index_engine", "multilingual_v3");
    }

    Ok(())
}

pub fn get_meta(db: &Connection, key: &str) -> Option<String> {
    db.query_row(
        "SELECT value FROM meta WHERE key = ?1",
        params![key],
        |row| row.get::<_, String>(0),
    )
    .optional()
    .ok()
    .flatten()
}

pub fn set_meta(db: &Connection, key: &str, value: &str) {
    let _ = db.execute(
        "INSERT INTO meta(key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_db_migrates_meta_and_fts_triggers() {
        let db = open_db_at(":memory:").unwrap();
        assert_eq!(get_meta(&db, "nonexistent"), None);
        set_meta(&db, "k1", "v1");
        assert_eq!(get_meta(&db, "k1").as_deref(), Some("v1"));
        set_meta(&db, "k1", "v2");
        assert_eq!(get_meta(&db, "k1").as_deref(), Some("v2"));

        db.execute(
            "INSERT INTO files(path, mtime_ms, size, hash, title, frontmatter, indexed_at) VALUES ('a.md', 1, 10, 'h', 'A', NULL, 1)",
            [],
        )
        .unwrap();
        db.execute(
            "INSERT INTO blocks(path, heading_path, start_line, end_line, text, embedding) VALUES ('a.md', 'H', 1, 2, 'unique_fts_token', NULL)",
            [],
        )
        .unwrap();

        let count_after_insert: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM blocks_fts WHERE blocks_fts MATCH 'unique_fts_token'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count_after_insert, 1);

        db.execute("DELETE FROM blocks WHERE path = 'a.md'", []).unwrap();
        let count_after_delete: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM blocks_fts WHERE blocks_fts MATCH 'unique_fts_token'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count_after_delete, 0);
    }
}
