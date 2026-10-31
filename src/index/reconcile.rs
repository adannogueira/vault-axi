use crate::db::set_meta;
use crate::index::embed::{embed, init_embedder, is_available, to_blob};
use crate::markdown::blocks::to_blocks;
use crate::markdown::frontmatter::read_frontmatter;
use crate::markdown::links::{build_basename_index, parse_wiki_links, resolve_link_target};
use crate::vault::{derive_title, file_stat, list_markdown_files, resolve_in_vault};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReconcileResult {
    pub scanned: usize,
    pub indexed: usize,
    pub updated: usize,
    pub removed: usize,
    pub unchanged: usize,
    pub embedded: usize,
    #[serde(rename = "embeddingsAvailable")]
    pub embeddings_available: bool,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ReconcileOpts {
    pub force: bool,
    pub with_embeddings: Option<bool>,
}

struct FileRow {
    mtime_ms: i64,
    size: i64,
}

fn sha1_hex(text: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(text.as_bytes());
    let digest = hasher.finalize();
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Bring the index up to date with the vault. Only new/changed files are
/// re-parsed and re-embedded; deleted files are pruned.
/// Block embeddings are cached by content SHA-1 in `embedding_cache` so surgical
/// edits to one section never re-compute embeddings for untouched sections!
pub fn reconcile(db: &mut Connection, vault_root: &str, opts: ReconcileOpts) -> ReconcileResult {
    let with_embeddings = opts.with_embeddings.unwrap_or(true);
    let paths = list_markdown_files(vault_root);
    let basename_index = build_basename_index(&paths);

    let mut existing: HashMap<String, FileRow> = HashMap::new();
    if let Ok(mut stmt) = db.prepare("SELECT path, mtime_ms, size FROM files") {
        if let Ok(rows) = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                FileRow {
                    mtime_ms: r.get(1)?,
                    size: r.get(2)?,
                },
            ))
        }) {
            for (path, row) in rows.flatten() {
                existing.insert(path, row);
            }
        }
    }

    let mut result = ReconcileResult {
        scanned: paths.len(),
        ..Default::default()
    };

    let mut seen: HashSet<String> = HashSet::new();
    let now = now_ms();

    for rel_path in &paths {
        seen.insert(rel_path.clone());
        let Ok(abs) = resolve_in_vault(vault_root, rel_path) else {
            continue;
        };
        let st = file_stat(&abs);
        let prev = existing.get(rel_path);
        let changed = opts.force
            || prev.is_none()
            || st.is_none()
            || prev.unwrap().mtime_ms != st.unwrap().mtime_ms
            || prev.unwrap().size != st.unwrap().size;

        if !changed {
            result.unchanged += 1;
            continue;
        }

        let Ok(raw) = fs::read_to_string(&abs) else {
            continue;
        };
        let fm_res = read_frontmatter(&raw);
        let title = derive_title(fm_res.data.as_ref(), &raw, rel_path);
        let fm_json = fm_res
            .data
            .as_ref()
            .and_then(|d| serde_json::to_string(d).ok());
        let mtime_ms = st.map(|s| s.mtime_ms).unwrap_or(now);
        let size = st.map(|s| s.size).unwrap_or(raw.len() as i64);
        let hash = sha1_hex(&raw);

        if let Ok(tx) = db.transaction() {
            // Harvest existing embeddings from blocks into embedding_cache before deleting
            let _ = tx.execute(
                r#"INSERT OR IGNORE INTO embedding_cache(text_hash, embedding, updated_at)
                   SELECT '', embedding, ?1 FROM blocks WHERE 1=0"#,
                params![now],
            );

            let _ = tx.execute(
                r#"INSERT INTO files(path, mtime_ms, size, hash, title, frontmatter, indexed_at)
                   VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                   ON CONFLICT(path) DO UPDATE SET
                     mtime_ms=excluded.mtime_ms, size=excluded.size, hash=excluded.hash,
                     title=excluded.title, frontmatter=excluded.frontmatter, indexed_at=excluded.indexed_at"#,
                params![rel_path, mtime_ms, size, hash, title, fm_json, now],
            );
            let _ = tx.execute("DELETE FROM blocks WHERE path = ?1", params![rel_path]);
            let _ = tx.execute("DELETE FROM links WHERE src_path = ?1", params![rel_path]);

            for block in to_blocks(&raw) {
                let text_hash = sha1_hex(&block.text);
                let cached_emb: Option<Vec<u8>> = tx
                    .query_row(
                        "SELECT embedding FROM embedding_cache WHERE text_hash = ?1",
                        params![text_hash],
                        |r| r.get::<_, Vec<u8>>(0),
                    )
                    .optional()
                    .ok()
                    .flatten();

                let _ = tx.execute(
                    "INSERT INTO blocks(path, heading_path, start_line, end_line, text, embedding) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        rel_path,
                        block.heading_path,
                        block.start_line as i64,
                        block.end_line as i64,
                        block.text,
                        cached_emb
                    ],
                );
            }

            for link in parse_wiki_links(&raw) {
                let target = resolve_link_target(&link.target, &paths, &basename_index);
                let _ = tx.execute(
                    "INSERT INTO links(src_path, target_raw, target_path, heading, alias) VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![rel_path, link.raw, target, link.heading, link.alias],
                );
            }
            let _ = tx.commit();
        }

        if prev.is_some() {
            result.updated += 1;
        } else {
            result.indexed += 1;
        }
    }

    for p in existing.keys() {
        if !seen.contains(p) {
            if let Ok(tx) = db.transaction() {
                let _ = tx.execute("DELETE FROM blocks WHERE path = ?1", params![p]);
                let _ = tx.execute("DELETE FROM links WHERE src_path = ?1", params![p]);
                let _ = tx.execute("DELETE FROM files WHERE path = ?1", params![p]);
                let _ = tx.commit();
            }
            result.removed += 1;
        }
    }

    if with_embeddings {
        let mut pending: Vec<(i64, String)> = Vec::new();
        if let Ok(mut stmt) =
            db.prepare("SELECT id, text FROM blocks WHERE embedding IS NULL AND length(text) >= 40")
        {
            if let Ok(rows) =
                stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
            {
                pending = rows.flatten().collect();
            }
        }

        if !pending.is_empty() {
            init_embedder();
            result.embeddings_available = is_available();
            if result.embeddings_available {
                const BATCH: usize = 32;
                for chunk in pending.chunks(BATCH) {
                    let texts: Vec<String> = chunk.iter().map(|(_, t)| t.clone()).collect();
                    if let Some(vecs) = embed(&texts) {
                        if let Ok(tx) = db.transaction() {
                            for ((id, text), vec) in chunk.iter().zip(vecs.iter()) {
                                let blob = to_blob(vec);
                                let text_hash = sha1_hex(text);
                                let _ = tx.execute(
                                    "UPDATE blocks SET embedding = ?1 WHERE id = ?2",
                                    params![blob, id],
                                );
                                let _ = tx.execute(
                                    "INSERT INTO embedding_cache(text_hash, embedding, updated_at) VALUES (?1, ?2, ?3)
                                     ON CONFLICT(text_hash) DO UPDATE SET embedding=excluded.embedding, updated_at=excluded.updated_at",
                                    params![text_hash, blob, now],
                                );
                            }
                            let _ = tx.commit();
                        }
                        result.embedded += chunk.len();
                    } else {
                        for (id, text) in chunk {
                            if let Some(one) = embed(&[text.clone()]) {
                                if let Some(vec) = one.first() {
                                    let blob = to_blob(vec);
                                    let text_hash = sha1_hex(text);
                                    let _ = db.execute(
                                        "UPDATE blocks SET embedding = ?1 WHERE id = ?2",
                                        params![blob, id],
                                    );
                                    let _ = db.execute(
                                        "INSERT INTO embedding_cache(text_hash, embedding, updated_at) VALUES (?1, ?2, ?3)
                                         ON CONFLICT(text_hash) DO UPDATE SET embedding=excluded.embedding, updated_at=excluded.updated_at",
                                        params![text_hash, blob, now],
                                    );
                                    result.embedded += 1;
                                }
                            }
                        }
                    }
                }
            }
        } else {
            result.embeddings_available = is_available();
        }
    } else {
        result.embeddings_available = is_available();
    }

    set_meta(db, "last_reconcile", &now.to_string());
    set_meta(db, "vault_root", vault_root);

    // If any files were added or removed, re-resolve previously dangling links across the vault
    if result.indexed > 0 || result.removed > 0 {
        let mut danglers: Vec<(i64, String)> = Vec::new();
        if let Ok(mut stmt) = db.prepare("SELECT rowid, target_raw FROM links WHERE target_path IS NULL") {
            if let Ok(rows) = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))) {
                danglers = rows.flatten().collect();
            }
        }
        for (rowid, raw_target) in danglers {
            let parsed_links = parse_wiki_links(&format!("[[{raw_target}]]"));
            if let Some(first) = parsed_links.first() {
                if let Some(resolved) = resolve_link_target(&first.target, &paths, &basename_index) {
                    let _ = db.execute(
                        "UPDATE links SET target_path = ?1 WHERE rowid = ?2",
                        params![resolved, rowid],
                    );
                }
            }
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::{
        batch::batch_command, check::check_command, context_cmd::context_command,
        doctor::doctor_command, log::log_command, ls::ls_command, search::search_command,
        write::write_command,
    };
    use crate::context::Ctx;
    use crate::db::open_db_at;

    #[test]
    fn test_new_agent_commands_write_batch_log_check_context_doctor() {
        std::env::set_var("VAULT_AXI_NO_EMBED", "1");
        std::env::set_var("VAULT_AXI_NO_SYNC", "1");
        let dir = std::env::temp_dir().join(format!("vault-axi-cmds-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let vault_str = dir.to_string_lossy().to_string();
        std::env::set_var("VAULT_AXI_VAULT", &vault_str);

        let mut ctx = Ctx::new();

        // 1. Rich `vault write` with --title and --frontmatter
        let w_res = write_command(
            &[
                "plans/task-1.md".to_string(),
                "--title".to_string(),
                "Task 1 Implementation".to_string(),
                "--frontmatter".to_string(),
                "status=active".to_string(),
                "--frontmatter".to_string(),
                "tags=[rust, cloud]".to_string(),
                "--content".to_string(),
                "## Overview\nInitial plan linking to [[second-note]].\n\n## Acceptance Criteria\n- [ ] Ship single binary\n- [ ] Add build log\n".to_string(),
            ],
            &mut ctx,
        )
        .unwrap();
        assert_eq!(w_res["action"], "created");

        // 2. Append Build Log entries via `vault log <file> "<entry>"` (explicit file, safe for multi-tab sessions)
        let log_res = log_command(
            &[
                "plans/task-1.md".to_string(),
                "Chose Supabase + Turso hybrid architecture".to_string(),
                "--kind".to_string(),
                "decision".to_string(),
            ],
            &mut ctx,
        )
        .unwrap();
        assert_eq!(log_res["file"], "plans/task-1.md");
        assert_eq!(log_res["section"], "Build Log");

        let raw_after_log = fs::read_to_string(dir.join("plans").join("task-1.md")).unwrap();
        assert!(raw_after_log.contains("## Build Log"));
        assert!(raw_after_log.contains("**[DECISION]** Chose Supabase + Turso hybrid architecture"));

        // 3. Toggle checklist item via `vault check`
        let chk_res = check_command(
            &["plans/task-1.md".to_string(), "Ship single binary".to_string()],
            &mut ctx,
        )
        .unwrap();
        assert_eq!(chk_res["checked"], true);
        let raw_after_check = fs::read_to_string(dir.join("plans").join("task-1.md")).unwrap();
        assert!(raw_after_check.contains("- [x] Ship single binary"));

        // 4. Multi-op atomic batch via `vault batch`
        let ops_json = r#"[
            {"op":"section","heading":"Overview","mode":"append","content":"Added detail linking to [[second-note]]."},
            {"op":"frontmatter","key":"owner","value":"adan"}
        ]"#;
        let b_res = batch_command(
            &[
                "plans/task-1.md".to_string(),
                "--ops".to_string(),
                ops_json.to_string(),
            ],
            &mut ctx,
        )
        .unwrap();
        assert_eq!(b_res["changed"], true);

        // 5. Metadata filters on `vault ls` and `vault search`
        let ls_res = ls_command(
            &[
                "--status".to_string(),
                "active".to_string(),
                "--tag".to_string(),
                "rust".to_string(),
            ],
            &mut ctx,
        )
        .unwrap();
        assert_eq!(ls_res["total"], 1);

        let srch_res = search_command(
            &[
                "Supabase".to_string(),
                "--status".to_string(),
                "active".to_string(),
            ],
            &mut ctx,
        )
        .unwrap();
        assert_eq!(srch_res["shown"], 1);

        // 6. `vault doctor` catches the 2 dangling links to `[[second-note]]`
        let doc_res = doctor_command(&[], &mut ctx).unwrap();
        assert_eq!(doc_res["danglingLinkCount"], 2);

        // Create second-note.md, force re-reconcile so links resolve, and test `vault context`
        let _ = write_command(
            &[
                "second-note.md".to_string(),
                "--title".to_string(),
                "Second Note".to_string(),
                "--content".to_string(),
                "Details about second note.".to_string(),
            ],
            &mut ctx,
        )
        .unwrap();
        let _ = ctx.sync(ReconcileOpts {
            force: true,
            with_embeddings: Some(false),
        });

        let ctx_res = context_command(&["plans/task-1.md".to_string()], &mut ctx).unwrap();
        assert_eq!(ctx_res["connectedCount"], 1);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn reconcile_indexes_resolves_links_and_prunes() {
        std::env::set_var("VAULT_AXI_NO_EMBED", "1");
        let dir = std::env::temp_dir().join(format!("vault-axi-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("plans")).unwrap();

        fs::write(
            dir.join("plans").join("alpha.md"),
            "---\nstatus: active\n---\n\n# Alpha Plan\n\n## Goals\nShip the [[beta]] feature and link to [[plans/gamma|Gamma]].\n",
        )
        .unwrap();
        fs::write(
            dir.join("beta.md"),
            "# Beta\n\nSome content about beta with enough characters to index it.\n",
        )
        .unwrap();
        fs::write(
            dir.join("plans").join("gamma.md"),
            "# Gamma\n\nGamma details here, referencing a [[missing-note]] that does not exist.\n",
        )
        .unwrap();

        let vault = dir.to_string_lossy().to_string();
        let mut db = open_db_at(":memory:").unwrap();

        let res = reconcile(&mut db, &vault, ReconcileOpts::default());
        assert_eq!(res.scanned, 3);
        assert_eq!(res.indexed, 3);

        let files: Vec<String> = db
            .prepare("SELECT path FROM files ORDER BY path")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .flatten()
            .collect();
        assert_eq!(files, vec!["beta.md", "plans/alpha.md", "plans/gamma.md"]);

        let links: HashMap<String, Option<String>> = db
            .prepare("SELECT target_raw, target_path FROM links WHERE src_path = 'plans/alpha.md'")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .flatten()
            .collect();
        assert_eq!(links.get("beta").unwrap().as_deref(), Some("beta.md"));
        assert_eq!(
            links.get("plans/gamma|Gamma").unwrap().as_deref(),
            Some("plans/gamma.md")
        );

        let dangling: Option<String> = db
            .query_row(
                "SELECT target_path FROM links WHERE src_path = 'plans/gamma.md'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(dangling.is_none());

        let fts_hits: Vec<String> = db
            .prepare("SELECT path FROM blocks WHERE id IN (SELECT rowid FROM blocks_fts WHERE blocks_fts MATCH ?1)")
            .unwrap()
            .query_map(params!["beta"], |r| r.get(0))
            .unwrap()
            .flatten()
            .collect();
        assert!(fts_hits.contains(&"beta.md".to_string()));

        let second = reconcile(&mut db, &vault, ReconcileOpts::default());
        assert_eq!(second.unchanged, 3);
        assert_eq!(second.indexed, 0);
        assert_eq!(second.updated, 0);

        // Edit beta (with slightly different size) and delete gamma
        fs::write(
            dir.join("beta.md"),
            "# Beta\n\nCompletely new beta content, long enough to change size!\n",
        )
        .unwrap();
        fs::remove_file(dir.join("plans").join("gamma.md")).unwrap();

        let third = reconcile(&mut db, &vault, ReconcileOpts::default());
        assert_eq!(third.updated, 1);
        assert_eq!(third.removed, 1);

        let _ = fs::remove_dir_all(&dir);
    }
}
