import Database from "better-sqlite3";
import { dbPath, ensureConfigDir } from "./config.js";

export type Db = Database.Database;

let handle: Db | null = null;

/** Open a database at an explicit path (used by tests), creating the schema. */
export function openDbAt(path: string): Db {
  const db = new Database(path);
  db.pragma("journal_mode = WAL");
  db.pragma("synchronous = NORMAL");
  migrate(db);
  return db;
}

/** Open (once) the index database, creating the schema if needed. */
export function openDb(): Db {
  if (handle) return handle;
  ensureConfigDir();
  handle = openDbAt(dbPath());
  return handle;
}

export function closeDb(): void {
  handle?.close();
  handle = null;
}

function migrate(db: Db): void {
  db.exec(`
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

    CREATE VIRTUAL TABLE IF NOT EXISTS blocks_fts USING fts5(
      text,
      heading_path,
      content='blocks',
      content_rowid='id',
      tokenize='porter unicode61'
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
  `);
}

export function getMeta(db: Db, key: string): string | null {
  const row = db.prepare("SELECT value FROM meta WHERE key = ?").get(key) as
    | { value: string }
    | undefined;
  return row?.value ?? null;
}

export function setMeta(db: Db, key: string, value: string): void {
  db.prepare(
    "INSERT INTO meta(key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
  ).run(key, value);
}
