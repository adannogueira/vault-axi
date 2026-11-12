use crate::db::set_meta;
use crate::index::embed::{embed, init_embedder, is_available, to_blob};
use crate::markdown::blocks::to_blocks;
use crate::markdown::frontmatter::read_frontmatter;
use crate::markdown::links::{build_basename_index, parse_wiki_links, resolve_link_target};
use crate::vault::{
    derive_title, file_stat, is_image_attachment, list_attachment_files, list_markdown_files,
    resolve_attachment_target, resolve_in_vault,
};
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
    let attachments = list_attachment_files(vault_root);
    let basename_index = build_basename_index(&paths);

    // Pre-scan image attachments so any newly added/updated image gets OCR'd and
    // triggers re-indexing of markdown notes that reference it.
    let mut newly_ocrd_images = false;
    for att in &attachments {
        if is_image_attachment(att) {
            if let Ok(abs_att) = resolve_in_vault(vault_root, att) {
                if let Ok(bytes) = fs::read(&abs_att) {
                    let mut h = Sha1::new();
                    h.update(&bytes);
                    let fh: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
                    let in_cache: bool = db
                        .query_row(
                            "SELECT 1 FROM ocr_cache WHERE hash = ?1",
                            params![fh],
                            |_| Ok(true),
                        )
                        .optional()
                        .ok()
                        .flatten()
                        .unwrap_or(false);
                    if !in_cache {
                        let _ = crate::ocr::extract_image_ocr(db, &abs_att);
                        newly_ocrd_images = true;
                    }
                }
            }
        }
    }

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
            || newly_ocrd_images
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
            let mut existing_embs: Vec<(String, Vec<u8>)> = Vec::new();
            if let Ok(mut stmt) = tx.prepare(
                "SELECT text, embedding FROM blocks WHERE path = ?1 AND embedding IS NOT NULL",
            ) {
                if let Ok(rows) = stmt.query_map(params![rel_path], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?))
                }) {
                    existing_embs = rows.flatten().collect();
                }
            }
            for (b_text, b_emb) in existing_embs {
                let th = sha1_hex(&b_text);
                let _ = tx.execute(
                    "INSERT OR IGNORE INTO embedding_cache(text_hash, embedding, updated_at) VALUES (?1, ?2, ?3)",
                    params![th, b_emb, now],
                );
            }

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

            let wiki_links = parse_wiki_links(&raw);
            let mut attached_images: Vec<String> = Vec::new();
            for link in &wiki_links {
                let target = resolve_link_target(&link.target, &paths, &basename_index);
                let _ = tx.execute(
                    "INSERT INTO links(src_path, target_raw, target_path, heading, alias) VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![rel_path, link.raw, target, link.heading, link.alias],
                );
                if target.is_none() {
                    if let Some(att_rel) = resolve_attachment_target(&link.raw, &attachments) {
                        if is_image_attachment(&att_rel) && !attached_images.contains(&att_rel) {
                            attached_images.push(att_rel);
                        }
                    }
                }
            }

            // Also scan standard Markdown links `![alt](target)` for image attachments
            for part in raw.split("](").skip(1) {
                if let Some(close) = part.find(')') {
                    let md_target = part[..close].split_whitespace().next().unwrap_or("");
                    if let Some(att_rel) = resolve_attachment_target(md_target, &attachments) {
                        if is_image_attachment(&att_rel) && !attached_images.contains(&att_rel) {
                            attached_images.push(att_rel);
                        }
                    }
                }
            }

            for att_rel in attached_images {
                if let Ok(abs_att) = resolve_in_vault(vault_root, &att_rel) {
                    if let Some(ocr_text) = crate::ocr::extract_image_ocr(&tx, &abs_att) {
                        let trimmed = ocr_text.trim();
                        if !trimmed.is_empty() {
                            let block_text = format!("[Attachment: {att_rel}]\n{trimmed}");
                            let text_hash = sha1_hex(&block_text);
                            let cached_emb: Option<Vec<u8>> = tx
                                .query_row(
                                    "SELECT embedding FROM embedding_cache WHERE text_hash = ?1",
                                    params![text_hash],
                                    |r| r.get::<_, Vec<u8>>(0),
                                )
                                .optional()
                                .ok()
                                .flatten();
                            let heading_path = format!("Attachment OCR ({att_rel})");
                            let _ = tx.execute(
                                "INSERT INTO blocks(path, heading_path, start_line, end_line, text, embedding) VALUES (?1, ?2, 1, 1, ?3, ?4)",
                                params![rel_path, heading_path, block_text, cached_emb],
                            );
                        }
                    }
                }
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

    // Whenever files are added, moved, or removed, re-resolve `target_path` across all
    // links so untouched referrer files (whose mtime did not change) always point to the
    // current target paths.
    if result.indexed > 0 || result.removed > 0 || opts.force {
        let mut all_links: Vec<(i64, String, Option<String>)> = Vec::new();
        if let Ok(mut stmt) = db.prepare("SELECT id, target_raw, target_path FROM links") {
            if let Ok(rows) = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            }) {
                all_links = rows.flatten().collect();
            }
        }
        if !all_links.is_empty() {
            if let Ok(tx) = db.transaction() {
                for (link_id, target_raw, prev_target) in all_links {
                    let bracketed = format!("[[{target_raw}]]");
                    let parsed = parse_wiki_links(&bracketed);
                    let new_target = parsed
                        .first()
                        .and_then(|wl| resolve_link_target(&wl.target, &paths, &basename_index));
                    if new_target != prev_target {
                        let _ = tx.execute(
                            "UPDATE links SET target_path = ?1 WHERE id = ?2",
                            params![new_target, link_id],
                        );
                    }
                }
                let _ = tx.commit();
            }
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
                            if let Some(one) = embed(std::slice::from_ref(text)) {
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
        if let Ok(mut stmt) =
            db.prepare("SELECT rowid, target_raw FROM links WHERE target_path IS NULL")
        {
            if let Ok(rows) =
                stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
            {
                danglers = rows.flatten().collect();
            }
        }
        for (rowid, raw_target) in danglers {
            let parsed_links = parse_wiki_links(&format!("[[{raw_target}]]"));
            if let Some(first) = parsed_links.first() {
                if let Some(resolved) = resolve_link_target(&first.target, &paths, &basename_index)
                {
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
        batch::batch_command,
        check::check_command,
        context_cmd::context_command,
        delete::delete_command,
        doctor::doctor_command,
        home::home_command,
        insert::insert_command,
        links::links_command,
        log::log_command,
        ls::{ls_command, matches_metadata_filter, normalize_dir_filter, parse_since_ms},
        mcp::{dispatch_mcp_tool, tool_definitions},
        move_cmd::move_command,
        path::path_command,
        read::read_command,
        reindex::reindex_command,
        related::related_command,
        search::search_command,
        setup::setup_command,
        sync_cmd::{login_command, register_command, sync_command},
        update::update_command,
        write::write_command,
    };
    use crate::context::Ctx;
    use crate::db::open_db_at;
    use crate::index::embed::to_blob;
    use serde_json::json;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn make_vault(tag: &str) -> (PathBuf, Ctx) {
        std::env::set_var("VAULT_AXI_NO_EMBED", "1");
        std::env::set_var("VAULT_AXI_NO_SYNC", "1");
        let id = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("vault-axi-{tag}-{}-{id}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let db = open_db_at(":memory:").unwrap();
        let ctx = Ctx::with_vault_and_db(dir.to_string_lossy().to_string(), db);
        (dir, ctx)
    }

    fn svec(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn test_new_agent_commands_write_batch_log_check_context_doctor() {
        let (dir, mut ctx) = make_vault("cmds");

        // 1. Rich `vault write` with --title and --frontmatter
        let w_res = write_command(
            &svec(&[
                "plans/task-1.md",
                "--title",
                "Task 1 Implementation",
                "--frontmatter",
                "status=active",
                "--frontmatter",
                "tags=[rust, cloud]",
                "--content",
                "## Overview\nInitial plan linking to [[second-note]].\n\n## Acceptance Criteria\n- [ ] Ship single binary\n- [ ] Add build log\n",
            ]),
            &mut ctx,
        )
        .unwrap();
        assert_eq!(w_res["action"], "created");

        // 2. Append Build Log entries via `vault log <file> "<entry>"`
        let log_res = log_command(
            &svec(&[
                "plans/task-1.md",
                "Chose bundled SQLite FTS5 architecture",
                "--kind",
                "decision",
            ]),
            &mut ctx,
        )
        .unwrap();
        assert_eq!(log_res["file"], "plans/task-1.md");
        assert_eq!(log_res["section"], "Build Log");

        let raw_after_log = fs::read_to_string(dir.join("plans").join("task-1.md")).unwrap();
        assert!(raw_after_log.contains("## Build Log"));
        assert!(raw_after_log.contains("**[DECISION]** Chose bundled SQLite FTS5 architecture"));

        // 3. Toggle checklist item via `vault check`
        let chk_res =
            check_command(&svec(&["plans/task-1.md", "Ship single binary"]), &mut ctx).unwrap();
        assert_eq!(chk_res["checked"], true);
        let raw_after_check = fs::read_to_string(dir.join("plans").join("task-1.md")).unwrap();
        assert!(raw_after_check.contains("- [x] Ship single binary"));

        // 4. Multi-op atomic batch via `vault batch`
        let ops_json = r#"[
            {"op":"section","heading":"Overview","mode":"append","content":"Added detail linking to [[second-note]]."},
            {"op":"frontmatter","key":"owner","value":"adan"}
        ]"#;
        let b_res =
            batch_command(&svec(&["plans/task-1.md", "--ops", ops_json]), &mut ctx).unwrap();
        assert_eq!(b_res["changed"], true);

        // 5. Metadata filters on `vault ls` and `vault search`
        let ls_res = ls_command(&svec(&["--status", "active", "--tag", "rust"]), &mut ctx).unwrap();
        assert_eq!(ls_res["total"], 1);

        let srch_res = search_command(&svec(&["SQLite", "--status", "active"]), &mut ctx).unwrap();
        assert_eq!(srch_res["shown"], 1);

        // 6. `vault doctor` catches the 2 dangling links to `[[second-note]]`
        let doc_res = doctor_command(&[], &mut ctx).unwrap();
        assert_eq!(doc_res["danglingLinkCount"], 2);

        // Create second-note.md — reconcile automatically re-resolves previously dangling links!
        let _ = write_command(
            &svec(&[
                "second-note.md",
                "--title",
                "Second Note",
                "--content",
                "Details about second note.",
            ]),
            &mut ctx,
        )
        .unwrap();

        let doc_after = doctor_command(&[], &mut ctx).unwrap();
        assert_eq!(doc_after["danglingLinkCount"], 0);
        assert_eq!(doc_after["healthy"], true);

        let ctx_res = context_command(&svec(&["plans/task-1.md"]), &mut ctx).unwrap();
        assert_eq!(ctx_res["connectedCount"], 1);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn reconcile_preserves_embedding_cache_across_surgical_edits() {
        let (dir, mut ctx) = make_vault("emb-cache");
        let sec1 = "## Section One\nThis first section has more than forty characters so it is embeddable.";
        let sec2 = "## Section Two\nThis second section also has more than forty characters for embedding.";
        write_command(
            &svec(&["note.md", "--content", &format!("{sec1}\n\n{sec2}\n")]),
            &mut ctx,
        )
        .unwrap();

        // Manually attach a synthetic embedding to Section One in blocks
        let fake_emb = to_blob(&[0.25f32, 0.75, 0.0]);
        ctx.db()
            .unwrap()
            .execute(
                "UPDATE blocks SET embedding = ?1 WHERE path = 'note.md' AND heading_path = 'Section One'",
                params![fake_emb],
            )
            .unwrap();

        // Surgically edit Section Two only
        update_command(
            &svec(&[
                "note.md",
                "--section",
                "Section Two",
                "--mode",
                "replace",
                "--content",
                "Updated second section content with plenty of characters.",
            ]),
            &mut ctx,
        )
        .unwrap();

        // Verify Section One's embedding was harvested into embedding_cache and restored!
        let restored: Option<Vec<u8>> = ctx
            .db()
            .unwrap()
            .query_row(
                "SELECT embedding FROM blocks WHERE path = 'note.md' AND heading_path = 'Section One'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(restored, Some(fake_emb));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_home_path_and_reindex_commands() {
        let mut unconfigured = Ctx::with_vault("unused");
        unconfigured.vault_path = None;
        let h_unconf = home_command(&[], &mut unconfigured).unwrap();
        assert_eq!(h_unconf["status"], "not configured");
        assert!(unconfigured.require_vault().is_err());

        let (dir, mut ctx) = make_vault("home-path");
        let p_root = path_command(&[], &mut ctx).unwrap();
        assert_eq!(p_root["vault"], ctx.vault_path.clone().unwrap());

        let p_new = path_command(&svec(&["deep/sub/note"]), &mut ctx).unwrap();
        assert_eq!(p_new["file"], "deep/sub/note.md");
        assert_eq!(p_new["exists"], false);
        assert!(dir.join("deep").join("sub").is_dir());

        // Exact 40-char block vs 39-char block to kill `length(text) >= 40` -> `> 40` mutation
        // "# H1\n" is 5 chars + 35 'a's = 40 chars; "## H2\n" is 6 chars + 33 'b's = 39 chars
        let exact_40_body = format!("# H1\n{}\n\n## H2\n{}\n", "a".repeat(35), "b".repeat(33));
        write_command(
            &svec(&[
                "deep/sub/note.md",
                "--frontmatter",
                "status=active",
                "--content",
                &exact_40_body,
            ]),
            &mut ctx,
        )
        .unwrap();

        let p_exists = path_command(&svec(&["deep/sub/note"]), &mut ctx).unwrap();
        assert_eq!(p_exists["exists"], true);

        // Home when 0/1 blocks are embedded (only the 40-char block is embeddable, not the 39-char block)
        let h1 = home_command(&[], &mut ctx).unwrap();
        assert_eq!(h1["notes"], 1);
        assert_eq!(h1["blocks"], 2);
        assert_eq!(h1["embeddings"], "0/1 (run `vault reindex`)");
        assert_eq!(h1["recent"][0], "deep/sub/note.md (active)");

        // Attach embedding to the 40-char block so embedded == embeddable (1 == 1)
        ctx.db()
            .unwrap()
            .execute(
                "UPDATE blocks SET embedding = ?1 WHERE length(text) >= 40",
                params![to_blob(&[1.0f32, 0.0])],
            )
            .unwrap();
        let h2 = home_command(&[], &mut ctx).unwrap();
        assert_eq!(h2["embeddings"], "complete (1 blocks)");

        let re_res = reindex_command(&svec(&["--force", "--no-embed"]), &mut ctx).unwrap();
        assert_eq!(re_res["ok"], true);
        assert_eq!(re_res["scanned"], 1);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_read_command_modes_boundaries_and_errors() {
        let (dir, mut ctx) = make_vault("read");
        assert!(read_command(&[], &mut ctx).is_err());
        assert_eq!(
            read_command(&svec(&["missing.md"]), &mut ctx)
                .unwrap_err()
                .code,
            "NOT_FOUND"
        );

        // Note without frontmatter or headings, exactly 1200 chars vs 1201 chars
        let exact_1200 = "a".repeat(1200);
        fs::write(dir.join("exact.md"), &exact_1200).unwrap();
        let r_exact = read_command(&svec(&["exact.md"]), &mut ctx).unwrap();
        assert_eq!(r_exact["truncated"], false);
        assert!(r_exact.get("hint").is_none());
        assert_eq!(
            read_command(&svec(&["exact.md", "--outline"]), &mut ctx).unwrap()["outline"],
            "(no headings)"
        );
        assert_eq!(
            read_command(&svec(&["exact.md", "--frontmatter"]), &mut ctx).unwrap()["frontmatter"],
            "(none)"
        );

        let over_1201 = "b".repeat(1201);
        fs::write(dir.join("over.md"), &over_1201).unwrap();
        let r_over = read_command(&svec(&["over.md"]), &mut ctx).unwrap();
        assert_eq!(r_over["truncated"], true);
        assert_eq!(r_over["totalChars"], 1201);
        assert!(r_over.get("hint").is_some());

        let r_full = read_command(&svec(&["over.md", "--full"]), &mut ctx).unwrap();
        assert_eq!(r_full["truncated"], false);
        assert_eq!(r_full["content"], over_1201);

        // Note with frontmatter, sections, and lines
        let long_sec = "x".repeat(1250);
        let structured = format!(
            "---\nstatus: draft\n---\n# Title\n## Short\nLine A\nLine B\n## LongSec\n{long_sec}\n## Dup\n1\n## Dup\n2"
        );
        fs::write(dir.join("doc.md"), &structured).unwrap();

        let fm = read_command(&svec(&["doc.md", "--frontmatter"]), &mut ctx).unwrap();
        assert_eq!(fm["frontmatter"]["status"], "draft");

        let ol = read_command(&svec(&["doc.md", "--outline"]), &mut ctx).unwrap();
        assert_eq!(ol["headings"], 5);

        // --lines boundaries
        let l_single = read_command(&svec(&["doc.md", "--lines", "6-6"]), &mut ctx).unwrap();
        assert_eq!(l_single["content"], "Line A");
        let l_range = read_command(&svec(&["doc.md", "--lines", "6-7"]), &mut ctx).unwrap();
        assert_eq!(l_range["content"], "Line A\nLine B");
        let l_last = read_command(&svec(&["doc.md", "--lines", "13-13"]), &mut ctx).unwrap();
        assert_eq!(l_last["content"], "2");
        let l_past_eof = read_command(&svec(&["doc.md", "--lines", "50-60"]), &mut ctx).unwrap();
        assert_eq!(l_past_eof["content"], "");

        // Invalid --lines
        assert!(read_command(&svec(&["doc.md", "--lines", "0-2"]), &mut ctx).is_err());
        assert!(read_command(&svec(&["doc.md", "--lines", "5-3"]), &mut ctx).is_err());
        assert!(read_command(&svec(&["doc.md", "--lines", "bad"]), &mut ctx).is_err());

        // --section short, truncated, --full, not found, ambiguous
        let s_short = read_command(&svec(&["doc.md", "--section", "Short"]), &mut ctx).unwrap();
        assert_eq!(s_short["truncated"], false);
        let s_long = read_command(&svec(&["doc.md", "--section", "LongSec"]), &mut ctx).unwrap();
        assert_eq!(s_long["truncated"], true);
        assert!(s_long.get("hint").is_some());
        let s_long_full = read_command(
            &svec(&["doc.md", "--section", "LongSec", "--full"]),
            &mut ctx,
        )
        .unwrap();
        assert_eq!(s_long_full["truncated"], false);

        assert_eq!(
            read_command(&svec(&["doc.md", "--section", "Missing"]), &mut ctx)
                .unwrap_err()
                .code,
            "NOT_FOUND"
        );
        assert_eq!(
            read_command(&svec(&["doc.md", "--section", "Dup"]), &mut ctx)
                .unwrap_err()
                .code,
            "VALIDATION_ERROR"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_write_update_insert_batch_log_check_edge_cases() {
        let (dir, mut ctx) = make_vault("edit-edges");

        // write edge cases
        assert!(write_command(&[], &mut ctx).is_err());
        assert!(write_command(&svec(&["a.md"]), &mut ctx).is_err());
        assert!(write_command(
            &svec(&["a.md", "--frontmatter", "=bad", "--content", "x"]),
            &mut ctx
        )
        .is_err());

        // write with --title only, and --title when content already starts with `# `
        write_command(&svec(&["title-only.md", "--title", "Only Title"]), &mut ctx).unwrap();
        assert_eq!(
            fs::read_to_string(dir.join("title-only.md")).unwrap(),
            "# Only Title\n"
        );

        write_command(
            &svec(&[
                "has-h1.md",
                "--title",
                "Other",
                "--content",
                "# Existing H1\nBody",
            ]),
            &mut ctx,
        )
        .unwrap();
        let has_h1_raw = fs::read_to_string(dir.join("has-h1.md")).unwrap();
        assert_eq!(has_h1_raw, "# Existing H1\nBody\n");

        // overwrite without --force fails; with --force succeeds
        assert!(write_command(&svec(&["has-h1.md", "--content", "new"]), &mut ctx).is_err());
        let ow = write_command(
            &svec(&[
                "has-h1.md",
                "--content",
                "# Doc\n\n## Sec\nBody\n",
                "--force",
            ]),
            &mut ctx,
        )
        .unwrap();
        assert_eq!(ow["action"], "overwritten");

        // update edge cases
        assert!(update_command(&[], &mut ctx).is_err());
        assert_eq!(
            update_command(&svec(&["nope.md", "--frontmatter", "a=1"]), &mut ctx)
                .unwrap_err()
                .code,
            "NOT_FOUND"
        );
        assert!(update_command(&svec(&["has-h1.md"]), &mut ctx).is_err());
        assert!(update_command(
            &svec(&[
                "has-h1.md",
                "--section",
                "Sec",
                "--mode",
                "bad",
                "--content",
                "x"
            ]),
            &mut ctx
        )
        .is_err());
        assert!(update_command(&svec(&["has-h1.md", "--anchor", "Body"]), &mut ctx).is_err());
        assert!(update_command(&svec(&["has-h1.md", "--frontmatter", "=bad"]), &mut ctx).is_err());

        // update no-op returns changed: false
        let u_noop = update_command(
            &svec(&[
                "has-h1.md",
                "--section",
                "Sec",
                "--mode",
                "replace",
                "--content",
                "Body",
            ]),
            &mut ctx,
        )
        .unwrap();
        assert_eq!(u_noop["changed"], false);

        // update prepend, anchor, and frontmatter set/unset
        let u_prep = update_command(
            &svec(&[
                "has-h1.md",
                "--section",
                "Sec",
                "--mode",
                "prepend",
                "--content",
                "Prepended",
            ]),
            &mut ctx,
        )
        .unwrap();
        assert_eq!(u_prep["changed"], true);

        let u_anc = update_command(
            &svec(&["has-h1.md", "--anchor", "Prepended", "--replace", "Swapped"]),
            &mut ctx,
        )
        .unwrap();
        assert_eq!(u_anc["changed"], true);

        update_command(
            &svec(&[
                "has-h1.md",
                "--frontmatter",
                "status=active",
                "--frontmatter",
                "temp=1",
            ]),
            &mut ctx,
        )
        .unwrap();
        update_command(&svec(&["has-h1.md", "--frontmatter", "temp"]), &mut ctx).unwrap();
        let fm_after = read_command(&svec(&["has-h1.md", "--frontmatter"]), &mut ctx).unwrap();
        assert_eq!(fm_after["frontmatter"]["status"], "active");
        assert!(fm_after["frontmatter"].get("temp").is_none());

        // insert command: before-heading, after-heading, prepend, append, and empty content no-op
        assert!(insert_command(&[], &mut ctx).is_err());
        assert!(insert_command(&svec(&["has-h1.md", "--content", "x"]), &mut ctx).is_err());
        let ins_empty = insert_command(
            &svec(&["has-h1.md", "--append", "--content", "   "]),
            &mut ctx,
        )
        .unwrap();
        assert_eq!(ins_empty["changed"], false);

        insert_command(
            &svec(&[
                "has-h1.md",
                "--before-heading",
                "Sec",
                "--content",
                "## BeforeSec\nB",
            ]),
            &mut ctx,
        )
        .unwrap();
        insert_command(
            &svec(&[
                "has-h1.md",
                "--after-heading",
                "Sec",
                "--content",
                "AfterLine",
            ]),
            &mut ctx,
        )
        .unwrap();
        insert_command(
            &svec(&["has-h1.md", "--prepend", "--content", "Preamble line"]),
            &mut ctx,
        )
        .unwrap();
        insert_command(
            &svec(&["has-h1.md", "--append", "--content", "Tail line"]),
            &mut ctx,
        )
        .unwrap();

        // batch command: all ops + atomicity when a later op fails
        let before_failed_batch = fs::read_to_string(dir.join("has-h1.md")).unwrap();
        let bad_batch = r#"[
            {"op":"frontmatter","key":"status","value":"corrupted"},
            {"op":"anchor","old_str":"DEFINITELY_NOT_PRESENT","new_str":"x"}
        ]"#;
        assert!(batch_command(&svec(&["has-h1.md", "--ops", bad_batch]), &mut ctx).is_err());
        assert_eq!(
            fs::read_to_string(dir.join("has-h1.md")).unwrap(),
            before_failed_batch
        );

        let good_batch = r#"[
            {"op":"insert","where_anchor":"before-heading","heading":"Sec","content":"Inserted via batch"},
            {"op":"insert","where_anchor":"after-heading","heading":"Sec","content":"After via batch"},
            {"op":"insert","where_anchor":"prepend","content":"Top via batch"},
            {"op":"insert","where_anchor":"append","content":"Bottom via batch"},
            {"op":"frontmatter","key":"status","value":null}
        ]"#;
        let gb_res = batch_command(&svec(&["has-h1.md", "--ops", good_batch]), &mut ctx).unwrap();
        assert_eq!(gb_res["operationsApplied"], 5);

        // log command: creating new file when absent, custom section, empty message error
        assert!(log_command(&[], &mut ctx).is_err());
        assert!(log_command(&svec(&["plans/new-log.md", "   "]), &mut ctx).is_err());
        let l_new = log_command(
            &svec(&["plans/new-log.md", "First entry", "--section", "Custom Log"]),
            &mut ctx,
        )
        .unwrap();
        assert_eq!(l_new["section"], "Custom Log");
        assert!(fs::read_to_string(dir.join("plans").join("new-log.md"))
            .unwrap()
            .contains("## Custom Log"));

        // check command: *, +, - [X], --list, --uncheck, --all
        write_command(
            &svec(&[
                "tasks.md",
                "--content",
                "- [ ] Task alpha one\n* [ ] Task alpha two\n+ [X] Done item\n",
            ]),
            &mut ctx,
        )
        .unwrap();
        let list_res = check_command(&svec(&["tasks.md", "--list"]), &mut ctx).unwrap();
        assert_eq!(list_res["totalTasks"], 3);
        assert_eq!(list_res["tasks"][2]["checked"], true);

        // Ambiguous match without --all fails; with --all checks both
        assert!(check_command(&svec(&["tasks.md", "Task alpha"]), &mut ctx).is_err());
        let all_res = check_command(&svec(&["tasks.md", "Task alpha", "--all"]), &mut ctx).unwrap();
        assert_eq!(all_res["items"].as_array().unwrap().len(), 2);

        // Uncheck item
        let unchk =
            check_command(&svec(&["tasks.md", "Done item", "--uncheck"]), &mut ctx).unwrap();
        assert_eq!(unchk["checked"], false);
        assert_eq!(unchk["changed"], true);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_ls_search_related_links_move_delete_and_mcp() {
        let (dir, mut ctx) = make_vault("graph-search");

        // parse_since_ms & normalize_dir_filter adversarial tests
        assert!(parse_since_ms("7d").is_some());
        assert!(parse_since_ms("24h").is_some());
        assert!(parse_since_ms("2w").is_some());
        assert!(parse_since_ms("2026-01-15").is_some());
        assert!(parse_since_ms("-5d").is_none());
        assert!(parse_since_ms("-1h").is_none());
        assert!(parse_since_ms("-1w").is_none());
        assert!(parse_since_ms("999999999999999999d").is_none());
        assert!(parse_since_ms("not-a-date").is_none());
        assert_eq!(
            normalize_dir_filter("./plans\\sub/"),
            Some("plans/sub".to_string())
        );
        assert_eq!(normalize_dir_filter("/"), None);

        // Create notes in `plans_v1` and `plansXv1` to verify SQL LIKE `_` wildcard is not triggered
        write_command(
            &svec(&[
                "plans_v1/alpha.md",
                "--title",
                "Alpha",
                "--frontmatter",
                "status=active",
                "--frontmatter",
                "tags=single_tag",
                "--content",
                "## Goals\nLink to [[beta#Details|Beta Alias]] and [[missing]]. Also inline `[[beta]]`.",
            ]),
            &mut ctx,
        )
        .unwrap();
        write_command(
            &svec(&[
                "plansXv1/other.md",
                "--title",
                "Other",
                "--frontmatter",
                "status=draft",
                "--content",
                "## Body\nUnrelated content in plansXv1 linking back to [[plans_v1/alpha]].",
            ]),
            &mut ctx,
        )
        .unwrap();
        write_command(
            &svec(&[
                "beta.md",
                "--title",
                "Beta",
                "--content",
                "## Details\nBeta details with enough text for searching.",
            ]),
            &mut ctx,
        )
        .unwrap();

        // ls with underscore folder must only return `plans_v1/alpha.md`, NOT `plansXv1/other.md`
        let ls_u = ls_command(&svec(&["plans_v1"]), &mut ctx).unwrap();
        assert_eq!(ls_u["total"], 1);
        assert_eq!(ls_u["files"][0]["path"], "plans_v1/alpha.md");

        // ls string tag match & invalid --since error
        let ls_tag = ls_command(&svec(&["--tag", "SINGLE_TAG"]), &mut ctx).unwrap();
        assert_eq!(ls_tag["total"], 1);
        assert!(ls_command(&svec(&["--since", "invalid"]), &mut ctx).is_err());

        // ls --limit pagination note
        let ls_lim = ls_command(&svec(&["--limit", "1"]), &mut ctx).unwrap();
        assert_eq!(ls_lim["total"], 3);
        assert_eq!(ls_lim["shown"], 1);
        assert!(ls_lim.get("note").is_some());

        // links command
        let lnk = links_command(&svec(&["plans_v1/alpha.md"]), &mut ctx).unwrap();
        assert_eq!(lnk["outboundCount"], 2);
        assert_eq!(lnk["backlinkCount"], 1);

        // related command: test graph + semantic score boundary (0.5)
        let v_alpha = to_blob(&[1.0f32, 0.0, 0.0]);
        let v_beta = to_blob(&[0.5f32, 0.8660254, 0.0]); // cosine == 0.5 with alpha (linked -> link+semantic)
        let v_other = to_blob(&[0.9f32, 0.4358899, 0.0]); // cosine == 0.9 with alpha
        ctx.db()
            .unwrap()
            .execute(
                "UPDATE blocks SET embedding = ?1 WHERE path = 'plans_v1/alpha.md'",
                params![v_alpha],
            )
            .unwrap();
        ctx.db()
            .unwrap()
            .execute(
                "UPDATE blocks SET embedding = ?1 WHERE path = 'beta.md'",
                params![v_beta],
            )
            .unwrap();
        ctx.db()
            .unwrap()
            .execute(
                "UPDATE blocks SET embedding = ?1 WHERE path = 'plansXv1/other.md'",
                params![v_other],
            )
            .unwrap();

        let rel = related_command(&svec(&["plans_v1/alpha.md"]), &mut ctx).unwrap();
        let rel_arr = rel["related"].as_array().unwrap();
        assert_eq!(rel_arr.len(), 2);
        let beta_rel = rel_arr.iter().find(|r| r["path"] == "beta.md").unwrap();
        assert_eq!(beta_rel["via"], "link+semantic");

        // Now set beta's cosine similarity to 0.4 (< 0.5) to kill `sem >= 0.5` -> `sem > 0.0` mutation,
        // and verify bidirectional link (`link+backlink`) on other.md when alpha also links to other.md!
        let v_beta_low = to_blob(&[0.4f32, 0.9165151, 0.0]);
        ctx.db()
            .unwrap()
            .execute(
                "UPDATE blocks SET embedding = ?1 WHERE path = 'beta.md'",
                params![v_beta_low],
            )
            .unwrap();
        let rel2 = related_command(&svec(&["plans_v1/alpha.md"]), &mut ctx).unwrap();
        let rel2_arr = rel2["related"].as_array().unwrap();
        let beta_rel2 = rel2_arr.iter().find(|r| r["path"] == "beta.md").unwrap();
        assert_eq!(beta_rel2["via"], "link");

        // related --limit 1
        let rel_lim =
            related_command(&svec(&["plans_v1/alpha.md", "--limit", "1"]), &mut ctx).unwrap();
        assert_eq!(rel_lim["shown"], 1);

        // context command: test --limit 1 and --full
        let ctx_lim = context_command(
            &svec(&["plans_v1/alpha.md", "--limit", "1", "--full"]),
            &mut ctx,
        )
        .unwrap();
        assert_eq!(ctx_lim["connectedCount"], 1);

        // doctor command: test healthy=false when only dangling (no orphans) vs only orphan (no dangling)
        // Currently: alpha -> beta, other -> alpha (all 3 connected, 0 orphans), but alpha -> [[missing]] (1 dangling)
        let doc_dangling_only = doctor_command(&[], &mut ctx).unwrap();
        assert_eq!(doc_dangling_only["orphanCount"], 0);
        assert_eq!(doc_dangling_only["danglingLinkCount"], 1);
        assert_eq!(doc_dangling_only["healthy"], false);

        // Self-linking orphan note (`self.md` linking only to `[[self]]`) must still be flagged as an orphan!
        write_command(
            &svec(&["self.md", "--content", "# Self\nLinks only to [[self]].\n"]),
            &mut ctx,
        )
        .unwrap();
        // Fix the dangling link by creating missing.md linking to beta
        write_command(
            &svec(&[
                "missing.md",
                "--frontmatter",
                "status=ok",
                "--content",
                "Links to [[beta]].\n",
            ]),
            &mut ctx,
        )
        .unwrap();
        let doc_orphan_only = doctor_command(&[], &mut ctx).unwrap();
        assert_eq!(doc_orphan_only["danglingLinkCount"], 0);
        assert_eq!(doc_orphan_only["orphanCount"], 1);
        assert_eq!(doc_orphan_only["orphanNotes"][0], "self.md");
        assert_eq!(doc_orphan_only["healthy"], false);

        // Deleting self-linking note `self.md` must NOT warn about dangling links (only linked to itself)
        let del_self = delete_command(&svec(&["self.md"]), &mut ctx).unwrap();
        assert_eq!(del_self["deleted"], true);
        assert!(del_self.get("warning").is_none());

        // move command: rewrites links to beta.md -> archive/beta-new.md, preserving inline `[[beta]]`
        let mv_same = move_command(&svec(&["beta.md", "beta.md"]), &mut ctx).unwrap();
        assert_eq!(mv_same["changed"], false);

        let mv = move_command(&svec(&["beta.md", "archive/beta-new.md"]), &mut ctx).unwrap();
        assert_eq!(mv["changed"], true);
        assert_eq!(mv["linksRewritten"], 2);
        let alpha_raw = fs::read_to_string(dir.join("plans_v1").join("alpha.md")).unwrap();
        assert!(alpha_raw.contains("[[beta-new#Details|Beta Alias]]"));
        assert!(alpha_raw.contains("`[[beta]]`"));

        // delete command: warns about incoming backlinks from plans_v1/alpha.md and missing.md
        let del = delete_command(&svec(&["archive/beta-new.md"]), &mut ctx).unwrap();
        assert_eq!(del["deleted"], true);
        assert!(del.get("warning").is_some());
        let del_again = delete_command(&svec(&["archive/beta-new.md"]), &mut ctx).unwrap();
        assert_eq!(del_again["deleted"], false);

        // search preview boundary (PREVIEW = 220 chars) and --limit 1 and --full
        // "# S\n" (4 chars) + 216 'z's -> collapsed "# S zzz..." is 4 + 216 = 220 chars (not truncated)
        // "## L\n" (5 chars) + 216 'q's -> collapsed "## L qqq..." is 5 + 216 = 221 chars (truncated with `…`)
        let s220 = format!("# S\n{}\n\n## L\n{}\n", "z".repeat(216), "q".repeat(216));
        write_command(&svec(&["preview-bound.md", "--content", &s220]), &mut ctx).unwrap();
        let srch_220 = search_command(&svec(&["zzzzzz", "--mode", "fts"]), &mut ctx).unwrap();
        let p_220 = srch_220["results"][0]["preview"].as_str().unwrap();
        assert_eq!(p_220.chars().count(), 220);
        assert!(!p_220.ends_with('…'));

        let srch_221 = search_command(&svec(&["qqqqqq", "--mode", "fts"]), &mut ctx).unwrap();
        let p_221 = srch_221["results"][0]["preview"].as_str().unwrap();
        assert!(p_221.ends_with('…'));

        let srch_221_full =
            search_command(&svec(&["qqqqqq", "--mode", "fts", "--full"]), &mut ctx).unwrap();
        let p_221_full = srch_221_full["results"][0]["preview"].as_str().unwrap();
        assert!(!p_221_full.ends_with('…'));

        let srch_lim1 =
            search_command(&svec(&["plansXv1 OR beta", "--limit", "1"]), &mut ctx).unwrap();
        assert_eq!(srch_lim1["shown"], 1);

        // search validation errors
        assert!(search_command(&[], &mut ctx).is_err());
        assert!(search_command(&svec(&["alpha", "--mode", "bogus"]), &mut ctx).is_err());
        assert!(search_command(&svec(&["alpha", "--since", "bogus"]), &mut ctx).is_err());

        // MCP dispatch & tool definitions (including values starting with `--`)
        let tools = tool_definitions();
        assert_eq!(tools.as_array().unwrap().len(), 25);

        let mcp_w = dispatch_mcp_tool(
            "vault_write",
            &json!({
                "file": "mcp-note.md",
                "title": "--Weird Title",
                "content": "--force is a flag",
                "frontmatter": {"status": "active"}
            }),
            &mut ctx,
        )
        .unwrap();
        assert_eq!(mcp_w["action"], "created");

        let mcp_r = dispatch_mcp_tool(
            "vault_read",
            &json!({"file": "mcp-note.md", "full": true}),
            &mut ctx,
        )
        .unwrap();
        assert!(mcp_r["content"]
            .as_str()
            .unwrap()
            .contains("--force is a flag"));

        // sync/register/login validation errors when unconfigured
        assert!(register_command(&[], &mut ctx).is_err());
        assert!(login_command(&[], &mut ctx).is_err());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_setup_and_sync_config_flows() {
        let id = COUNTER.fetch_add(1, Ordering::SeqCst);
        let home_dir = std::env::temp_dir().join(format!("vx-home-{}-{id}", std::process::id()));
        let vault_dir = home_dir.join("my-vault");
        let _ = fs::remove_dir_all(&home_dir);
        fs::create_dir_all(&home_dir).unwrap();
        std::env::set_var("VAULT_AXI_HOME", &home_dir);

        let mut ctx = Ctx::with_vault(vault_dir.to_string_lossy().to_string());

        let s_noop = setup_command(&[], &mut ctx).unwrap();
        assert_eq!(s_noop["help"], "Nothing to do");

        // sync when no remote is configured must return VALIDATION_ERROR
        assert_eq!(
            sync_command(&[], &mut ctx).unwrap_err().code,
            "VALIDATION_ERROR"
        );

        // S3 without --endpoint must error
        assert!(setup_command(&svec(&["--remote", "s3://bucket/prefix"]), &mut ctx).is_err());

        // S3 with --endpoint succeeds
        let s_s3 = setup_command(
            &svec(&[
                "--vault",
                &vault_dir.to_string_lossy(),
                "--remote",
                "s3://my-bucket/notes",
                "--endpoint",
                "https://r2.example.com",
                "--auto-sync=false",
            ]),
            &mut ctx,
        )
        .unwrap();
        assert_eq!(s_s3["remote"], "s3");
        assert_eq!(s_s3["autoSync"], false);

        // Git remote setup
        let s_git = setup_command(
            &svec(&[
                "--remote",
                "git:https://github.com/example/vault.git",
                "--branch",
                "main",
            ]),
            &mut ctx,
        )
        .unwrap();
        assert_eq!(s_git["remote"], "git");

        // MFA when not logged into cloud returns AUTH_ERROR
        assert_eq!(
            login_command(&svec(&["--mfa-enroll"]), &mut ctx)
                .unwrap_err()
                .code,
            "AUTH_ERROR"
        );

        // matches_metadata_filter direct unit coverage
        assert!(matches_metadata_filter(None, 100, None, None, Some(100)));
        assert!(!matches_metadata_filter(None, 99, None, None, Some(100)));
        assert!(!matches_metadata_filter(
            None,
            100,
            Some("active"),
            None,
            None
        ));
        assert!(!matches_metadata_filter(
            Some("not-json"),
            100,
            Some("active"),
            None,
            None
        ));

        let _ = fs::remove_dir_all(&home_dir);
        std::env::remove_var("VAULT_AXI_HOME");
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

    #[test]
    fn test_bom_batch_hash_tag_filter_and_sequential_moves() {
        std::env::set_var("VAULT_AXI_NO_EMBED", "1");
        std::env::set_var("VAULT_AXI_NO_SYNC", "1");

        let dir = std::env::temp_dir().join(format!(
            "vault_axi_v1_findings_test_{}_{}",
            std::process::id(),
            now_ms()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("f1")).unwrap();

        let vault = dir.to_string_lossy().to_string();
        let db = open_db_at(":memory:").unwrap();
        let mut ctx = crate::context::Ctx::with_vault_and_db(&vault, db);

        // 1. Create a note with `#work` tag and a heading containing `/`
        crate::commands::write::write_command(
            &[
                "Supabase.md".to_string(),
                "--frontmatter".to_string(),
                "tags=[#work, #backend]".to_string(),
                "--content".to_string(),
                "# Supabase\n\n## Multi Tenant\n\n#### Usuários / Labels\nOld body\n".to_string(),
            ],
            &mut ctx,
        )
        .unwrap();

        // 2. Tag filtering matches both `--tag work` and `--tag "#work"`
        let ls_plain =
            crate::commands::ls::ls_command(&["--tag".to_string(), "work".to_string()], &mut ctx)
                .unwrap();
        assert_eq!(ls_plain["total"], 1);

        let ls_hash =
            crate::commands::ls::ls_command(&["--tag".to_string(), "#work".to_string()], &mut ctx)
                .unwrap();
        assert_eq!(ls_hash["total"], 1);

        // 3. UTF-8 BOM in `--from ops.json` for `vault batch` + section with `/`
        let ops_file = dir.join("ops_bom.json");
        let bom_ops = "\u{feff}[{\"op\":\"section\",\"heading\":\"Multi Tenant/Usuários / Labels\",\"mode\":\"replace\",\"content\":\"New body via BOM batch\"}]";
        fs::write(&ops_file, bom_ops).unwrap();

        let batch_res = crate::commands::batch::batch_command(
            &[
                "Supabase.md".to_string(),
                "--from".to_string(),
                ops_file.to_string_lossy().to_string(),
            ],
            &mut ctx,
        )
        .unwrap();
        assert_eq!(batch_res["changed"], true);

        let read_sec = crate::commands::read::read_command(
            &[
                "Supabase.md".to_string(),
                "--section".to_string(),
                "Usuários / Labels".to_string(),
            ],
            &mut ctx,
        )
        .unwrap();
        assert_eq!(
            read_sec["content"],
            "#### Usuários / Labels\nNew body via BOM batch"
        );

        // 4. Sequential `vault move` where Move 1 changes folder (referrer text `[[target]]` unchanged)
        // and Move 2 renames the note (must still find referrer and rewrite `[[target]]` -> `[[renamed]]`)
        crate::commands::write::write_command(
            &[
                "f1/target.md".to_string(),
                "--content".to_string(),
                "# Target\nBody\n".to_string(),
            ],
            &mut ctx,
        )
        .unwrap();
        crate::commands::write::write_command(
            &[
                "referrer.md".to_string(),
                "--content".to_string(),
                "# Referrer\nSee [[target]] for details.\n".to_string(),
            ],
            &mut ctx,
        )
        .unwrap();

        // Move 1: f1/target.md -> f2/target.md (basename is still `target`, so referrer.md mtime doesn't change)
        let m1 = crate::commands::move_cmd::move_command(
            &["f1/target.md".to_string(), "f2/target.md".to_string()],
            &mut ctx,
        )
        .unwrap();
        assert_eq!(m1["referrers"], 1);
        assert_eq!(m1["linksRewritten"], 0);

        // Move 2: f2/target.md -> f2/renamed.md (must detect referrer.md and rewrite [[target]] -> [[renamed]])
        let m2 = crate::commands::move_cmd::move_command(
            &["f2/target.md".to_string(), "f2/renamed.md".to_string()],
            &mut ctx,
        )
        .unwrap();
        assert_eq!(m2["referrers"], 1);
        assert_eq!(m2["linksRewritten"], 1);

        let ref_after = fs::read_to_string(dir.join("referrer.md")).unwrap();
        assert!(ref_after.contains("[[renamed]]"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_milestone_v2_capture_trash_history_attach_ocr_and_crypto() {
        std::env::set_var("VAULT_AXI_NO_EMBED", "1");
        std::env::set_var("VAULT_AXI_NO_SYNC", "1");
        let dir = std::env::temp_dir().join(format!(
            "vault_axi_milestone_v2_{}_{}",
            std::process::id(),
            now_ms()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let vault_str = dir.to_string_lossy().to_string();
        let db = open_db_at(":memory:").unwrap();
        let mut ctx = Ctx::with_vault_and_db(&vault_str, db);

        // 1. Fast capture (`vault capture`) into daily note and custom `--to` note
        let cap1 = crate::commands::capture::capture_command(
            &[
                "Remember to benchmark AES-256-GCM throughput".to_string(),
                "--tag".to_string(),
                "security".to_string(),
            ],
            &mut ctx,
        )
        .unwrap();
        assert_eq!(cap1["ok"], true);
        let daily_file = cap1["file"].as_str().unwrap().to_string();
        assert!(daily_file.starts_with("daily/"));
        let daily_raw = fs::read_to_string(dir.join(&daily_file)).unwrap();
        assert!(daily_raw.contains("## Captures"));
        assert!(daily_raw.contains("Remember to benchmark AES-256-GCM throughput #security"));

        let cap2 = crate::commands::capture::capture_command(
            &[
                "Second thought on same day".to_string(),
                "--to".to_string(),
                "inbox/quick.md".to_string(),
                "--section".to_string(),
                "Quick Notes".to_string(),
            ],
            &mut ctx,
        )
        .unwrap();
        assert_eq!(cap2["file"], "inbox/quick.md");
        assert!(fs::read_to_string(dir.join("inbox/quick.md"))
            .unwrap()
            .contains("## Quick Notes"));

        // 2. Edits history (`vault history`) and encryption at rest in `note_versions`
        crate::commands::write::write_command(
            &[
                "specs/auth.md".to_string(),
                "--title".to_string(),
                "Auth Spec".to_string(),
                "--content".to_string(),
                "## Overview\nVersion one token LegacySecretTokenAlpha\n".to_string(),
            ],
            &mut ctx,
        )
        .unwrap();

        crate::commands::update::update_command(
            &[
                "specs/auth.md".to_string(),
                "--section".to_string(),
                "Overview".to_string(),
                "--mode".to_string(),
                "replace".to_string(),
                "--content".to_string(),
                "Version two token ModernScopedTokenBeta\n".to_string(),
            ],
            &mut ctx,
        )
        .unwrap();

        // Verify stored version content in SQLite is encrypted at rest (`enc:v1:...`)
        let raw_db_ver: String = ctx
            .db()
            .unwrap()
            .query_row(
                "SELECT content FROM note_versions WHERE path = 'specs/auth.md' AND version_num = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(raw_db_ver.starts_with("enc:v1:"));

        // Verify historical version 1 text ("LegacySecretTokenAlpha") does NOT pollute `vault search`
        let srch_old = crate::commands::search::search_command(
            &[
                "LegacySecretTokenAlpha".to_string(),
                "--mode".to_string(),
                "fts".to_string(),
            ],
            &mut ctx,
        )
        .unwrap();
        assert!(srch_old["results"]
            .as_str()
            .unwrap()
            .starts_with("0 matches"));

        let srch_curr = crate::commands::search::search_command(
            &[
                "ModernScopedTokenBeta".to_string(),
                "--mode".to_string(),
                "fts".to_string(),
            ],
            &mut ctx,
        )
        .unwrap();
        assert_eq!(srch_curr["shown"], 1);

        // List versions, diff v1, and restore v1
        let hist_list =
            crate::commands::history::history_command(&["specs/auth.md".to_string()], &mut ctx)
                .unwrap();
        assert_eq!(hist_list["versionsCount"], 2);

        let hist_diff = crate::commands::history::history_command(
            &[
                "specs/auth.md".to_string(),
                "--diff".to_string(),
                "1".to_string(),
            ],
            &mut ctx,
        )
        .unwrap();
        assert!(hist_diff["diffAgainstCurrent"].is_object());

        let hist_restore = crate::commands::history::history_command(
            &[
                "specs/auth.md".to_string(),
                "--restore".to_string(),
                "1".to_string(),
            ],
            &mut ctx,
        )
        .unwrap();
        assert_eq!(hist_restore["action"], "restored_version");
        assert!(fs::read_to_string(dir.join("specs/auth.md"))
            .unwrap()
            .contains("LegacySecretTokenAlpha"));

        // 3. Trash and restore (7-day soft delete + encrypted trash payload)
        let del_res =
            crate::commands::delete::delete_command(&["specs/auth.md".to_string()], &mut ctx)
                .unwrap();
        assert_eq!(del_res["trashed"], true);
        assert!(!dir.join("specs/auth.md").exists());

        let trash_ls = crate::commands::trash::trash_command(&[], &mut ctx).unwrap();
        assert_eq!(trash_ls["count"], 1);

        let rest_res =
            crate::commands::trash::restore_command(&["specs/auth.md".to_string()], &mut ctx)
                .unwrap();
        assert_eq!(rest_res["action"], "restored");
        assert!(dir.join("specs/auth.md").exists());

        // 4. Attachments + Local Image OCR indexing + `.obsidian/` config sync exclusion from search
        let ext_img = dir.join("temp_whiteboard.png");
        fs::write(&ext_img, b"\x89PNG\r\n\x1a\nmock-image-bytes").unwrap();
        std::env::set_var(
            "VAULT_AXI_MOCK_OCR",
            "DiagramArchitectureMicroserviceGateway99",
        );

        let att_res = crate::commands::attach::attach_command(
            &[
                "specs/auth.md".to_string(),
                ext_img.to_string_lossy().to_string(),
            ],
            &mut ctx,
        )
        .unwrap();
        assert_eq!(att_res["ok"], true);
        assert_eq!(att_res["attachment"], "assets/temp_whiteboard.png");
        assert_eq!(att_res["ocrExtracted"], true);
        let _ = fs::remove_file(&ext_img);

        let att_list =
            crate::commands::attach::attachments_command(&["specs/auth.md".to_string()], &mut ctx)
                .unwrap();
        assert_eq!(att_list["count"], 1);

        // Search finds `specs/auth.md` via the OCR text inside `assets/temp_whiteboard.png`!
        let srch_ocr = crate::commands::search::search_command(
            &[
                "DiagramArchitectureMicroserviceGateway99".to_string(),
                "--mode".to_string(),
                "fts".to_string(),
            ],
            &mut ctx,
        )
        .unwrap();
        assert_eq!(srch_ocr["shown"], 1);
        assert_eq!(srch_ocr["results"][0]["path"], "specs/auth.md");
        std::env::remove_var("VAULT_AXI_MOCK_OCR");

        // `.obsidian/` files are included in `list_all_syncable_files` for cloud sync,
        // but excluded from `vault ls` and `vault search`!
        fs::create_dir_all(dir.join(".obsidian")).unwrap();
        fs::write(
            dir.join(".obsidian/app.json"),
            r#"{"vimMode": true, "secretKeyword": "ObsidianInternalConfigToken88"}"#,
        )
        .unwrap();

        let syncable = crate::sync::list_all_syncable_files(&dir.to_string_lossy());
        assert!(syncable.contains(&".obsidian/app.json".to_string()));
        assert!(syncable.contains(&"assets/temp_whiteboard.png".to_string()));
        assert!(syncable.contains(&"specs/auth.md".to_string()));

        let srch_obs = crate::commands::search::search_command(
            &[
                "ObsidianInternalConfigToken88".to_string(),
                "--mode".to_string(),
                "fts".to_string(),
            ],
            &mut ctx,
        )
        .unwrap();
        assert!(srch_obs["results"]
            .as_str()
            .unwrap()
            .starts_with("0 matches"));

        let _ = fs::remove_dir_all(&dir);
    }
}
