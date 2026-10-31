//! Built-in Model Context Protocol (MCP) server over stdio.
//! Allows non-technical or desktop users to connect Claude Desktop, Cursor, or
//! any MCP client directly to the single `vault` binary (`vault mcp`) with zero Node/Python setup.

use crate::axi::AxiError;
use crate::commands::{
    batch::batch_command, check::check_command, context_cmd::context_command,
    delete::delete_command, doctor::doctor_command, home::home_command, insert::insert_command,
    links::links_command, log::log_command, ls::ls_command, move_cmd::move_command,
    path::path_command, read::read_command, reindex::reindex_command, related::related_command,
    search::search_command, sync_cmd::sync_command, update::update_command, write::write_command,
};
use crate::context::Ctx;
use serde_json::{json, Value};
use std::io::{self, BufRead, Write};
use toon_format::{encode, EncodeOptions};

pub fn mcp_command() -> Result<Value, AxiError> {
    let stdin = io::stdin();
    let mut stdout = io::stdout();

    for line in stdin.lock().lines() {
        let Ok(raw) = line else {
            break;
        };
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(req) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        let id = req.get("id").cloned();
        let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let params = req.get("params").cloned().unwrap_or_else(|| json!({}));

        // Notifications have no id
        if id.is_none() {
            continue;
        }
        let req_id = id.unwrap();

        let response = match method {
            "initialize" => json!({
                "jsonrpc": "2.0",
                "id": req_id,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {
                        "tools": {}
                    },
                    "serverInfo": {
                        "name": "vault-axi",
                        "version": "0.1.0"
                    }
                }
            }),
            "ping" => json!({
                "jsonrpc": "2.0",
                "id": req_id,
                "result": {}
            }),
            "tools/list" => json!({
                "jsonrpc": "2.0",
                "id": req_id,
                "result": {
                    "tools": tool_definitions()
                }
            }),
            "tools/call" => {
                let tool_name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
                let args_obj = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
                let mut ctx = Ctx::new();
                match dispatch_mcp_tool(tool_name, &args_obj, &mut ctx) {
                    Ok(val) => {
                        let text = encode(&val, &EncodeOptions::default())
                            .unwrap_or_else(|_| serde_json::to_string_pretty(&val).unwrap_or_default());
                        json!({
                            "jsonrpc": "2.0",
                            "id": req_id,
                            "result": {
                                "content": [{ "type": "text", "text": text }],
                                "isError": false
                            }
                        })
                    }
                    Err(err) => {
                        let err_val = json!({
                            "error": err.message,
                            "code": err.code,
                            "help": err.help
                        });
                        let text = encode(&err_val, &EncodeOptions::default())
                            .unwrap_or_else(|_| err.message.clone());
                        json!({
                            "jsonrpc": "2.0",
                            "id": req_id,
                            "result": {
                                "content": [{ "type": "text", "text": text }],
                                "isError": true
                            }
                        })
                    }
                }
            }
            _ => json!({
                "jsonrpc": "2.0",
                "id": req_id,
                "error": {
                    "code": -32601,
                    "message": format!("Method not found: {method}")
                }
            }),
        };

        if let Ok(line_out) = serde_json::to_string(&response) {
            let _ = writeln!(stdout, "{line_out}");
            let _ = stdout.flush();
        }
    }

    Ok(json!({ "ok": true }))
}

pub fn dispatch_mcp_tool(name: &str, args: &Value, ctx: &mut Ctx) -> Result<Value, AxiError> {
    let mut cli_args: Vec<String> = Vec::new();
    let str_arg = |k: &str| args.get(k).and_then(|v| v.as_str()).map(|s| s.to_string());
    let bool_arg = |k: &str| args.get(k).and_then(|v| v.as_bool()).unwrap_or(false);
    let int_arg = |k: &str| args.get(k).and_then(|v| v.as_i64()).map(|n| n.to_string());

    match name {
        "vault_home" => home_command(&[], ctx),
        "vault_path" => {
            if let Some(f) = str_arg("file").or_else(|| str_arg("note")) {
                cli_args.push(f);
            }
            path_command(&cli_args, ctx)
        }
        "vault_read" => {
            if let Some(f) = str_arg("file") {
                cli_args.push(f);
            }
            if bool_arg("outline") {
                cli_args.push("--outline".to_string());
            }
            if bool_arg("frontmatter") {
                cli_args.push("--frontmatter".to_string());
            }
            if bool_arg("full") {
                cli_args.push("--full".to_string());
            }
            if let Some(s) = str_arg("section") {
                cli_args.push(format!("--section={s}"));
            }
            if let Some(l) = str_arg("lines") {
                cli_args.push(format!("--lines={l}"));
            }
            read_command(&cli_args, ctx)
        }
        "vault_write" => {
            if let Some(f) = str_arg("file") {
                cli_args.push(f);
            }
            if let Some(t) = str_arg("title") {
                cli_args.push(format!("--title={t}"));
            }
            if let Some(c) = str_arg("content") {
                cli_args.push(format!("--content={c}"));
            }
            if bool_arg("force") {
                cli_args.push("--force".to_string());
            }
            if let Some(Value::Object(fm)) = args.get("frontmatter") {
                for (k, v) in fm {
                    let v_str = match v {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    cli_args.push(format!("--frontmatter={k}={v_str}"));
                }
            }
            write_command(&cli_args, ctx)
        }
        "vault_update" => {
            if let Some(f) = str_arg("file") {
                cli_args.push(f);
            }
            if let Some(s) = str_arg("section") {
                cli_args.push(format!("--section={s}"));
            }
            if let Some(m) = str_arg("mode") {
                cli_args.push(format!("--mode={m}"));
            }
            if let Some(c) = str_arg("content") {
                cli_args.push(format!("--content={c}"));
            }
            if let Some(a) = str_arg("anchor") {
                cli_args.push(format!("--anchor={a}"));
            }
            if let Some(r) = str_arg("replace") {
                cli_args.push(format!("--replace={r}"));
            }
            if let Some(Value::Object(fm)) = args.get("frontmatter") {
                for (k, v) in fm {
                    if v.is_null() {
                        cli_args.push(format!("--frontmatter={k}"));
                    } else if let Value::String(s) = v {
                        cli_args.push(format!("--frontmatter={k}={s}"));
                    } else {
                        cli_args.push(format!("--frontmatter={k}={v}"));
                    }
                }
            }
            update_command(&cli_args, ctx)
        }
        "vault_batch" => {
            if let Some(f) = str_arg("file") {
                cli_args.push(f);
            }
            if let Some(ops) = args.get("ops") {
                cli_args.push(format!("--ops={ops}"));
            }
            batch_command(&cli_args, ctx)
        }
        "vault_insert" => {
            if let Some(f) = str_arg("file") {
                cli_args.push(f);
            }
            if let Some(ah) = str_arg("after_heading") {
                cli_args.push(format!("--after-heading={ah}"));
            }
            if let Some(bh) = str_arg("before_heading") {
                cli_args.push(format!("--before-heading={bh}"));
            }
            if bool_arg("append") {
                cli_args.push("--append".to_string());
            }
            if bool_arg("prepend") {
                cli_args.push("--prepend".to_string());
            }
            if let Some(c) = str_arg("content") {
                cli_args.push(format!("--content={c}"));
            }
            insert_command(&cli_args, ctx)
        }
        "vault_log" => {
            if let Some(f) = str_arg("file") {
                cli_args.push(f);
            }
            if let Some(e) = str_arg("entry") {
                cli_args.push(format!("--content={e}"));
            }
            if let Some(k) = str_arg("kind") {
                cli_args.push(format!("--kind={k}"));
            }
            if let Some(s) = str_arg("section") {
                cli_args.push(format!("--section={s}"));
            }
            log_command(&cli_args, ctx)
        }
        "vault_check" => {
            if let Some(f) = str_arg("file") {
                cli_args.push(f);
            }
            if bool_arg("uncheck") {
                cli_args.push("--uncheck".to_string());
            }
            if bool_arg("all") {
                cli_args.push("--all".to_string());
            }
            if bool_arg("list") {
                cli_args.push("--list".to_string());
            }
            if let Some(t) = str_arg("task") {
                cli_args.push("--".to_string());
                cli_args.push(t);
            }
            check_command(&cli_args, ctx)
        }
        "vault_search" => {
            if let Some(m) = str_arg("mode") {
                cli_args.push(format!("--mode={m}"));
            }
            if let Some(l) = int_arg("limit") {
                cli_args.push(format!("--limit={l}"));
            }
            if let Some(f) = str_arg("folder") {
                cli_args.push(format!("--folder={f}"));
            }
            if let Some(st) = str_arg("status") {
                cli_args.push(format!("--status={st}"));
            }
            if let Some(tg) = str_arg("tag") {
                cli_args.push(format!("--tag={tg}"));
            }
            if let Some(sn) = str_arg("since") {
                cli_args.push(format!("--since={sn}"));
            }
            if bool_arg("full") {
                cli_args.push("--full".to_string());
            }
            if let Some(q) = str_arg("query") {
                cli_args.push("--".to_string());
                cli_args.push(q);
            }
            search_command(&cli_args, ctx)
        }
        "vault_context" => {
            if let Some(f) = str_arg("file") {
                cli_args.push(f);
            }
            if let Some(l) = int_arg("limit") {
                cli_args.push(format!("--limit={l}"));
            }
            if bool_arg("full") {
                cli_args.push("--full".to_string());
            }
            context_command(&cli_args, ctx)
        }
        "vault_related" => {
            if let Some(f) = str_arg("file") {
                cli_args.push(f);
            }
            if let Some(l) = int_arg("limit") {
                cli_args.push(format!("--limit={l}"));
            }
            related_command(&cli_args, ctx)
        }
        "vault_links" => {
            if let Some(f) = str_arg("file") {
                cli_args.push(f);
            }
            links_command(&cli_args, ctx)
        }
        "vault_ls" => {
            if let Some(d) = str_arg("dir") {
                cli_args.push(d);
            }
            if let Some(st) = str_arg("status") {
                cli_args.push(format!("--status={st}"));
            }
            if let Some(tg) = str_arg("tag") {
                cli_args.push(format!("--tag={tg}"));
            }
            if let Some(sn) = str_arg("since") {
                cli_args.push(format!("--since={sn}"));
            }
            if let Some(l) = int_arg("limit") {
                cli_args.push(format!("--limit={l}"));
            }
            ls_command(&cli_args, ctx)
        }
        "vault_move" => {
            if let Some(o) = str_arg("old_path") {
                cli_args.push(o);
            }
            if let Some(n) = str_arg("new_path") {
                cli_args.push(n);
            }
            if bool_arg("force") {
                cli_args.push("--force".to_string());
            }
            move_command(&cli_args, ctx)
        }
        "vault_delete" => {
            if let Some(f) = str_arg("file") {
                cli_args.push(f);
            }
            delete_command(&cli_args, ctx)
        }
        "vault_reindex" => {
            if bool_arg("force") {
                cli_args.push("--force".to_string());
            }
            if bool_arg("no_embed") {
                cli_args.push("--no-embed".to_string());
            }
            reindex_command(&cli_args, ctx)
        }
        "vault_doctor" => doctor_command(&[], ctx),
        "vault_sync" => {
            if bool_arg("pull") {
                cli_args.push("--pull".to_string());
            }
            if bool_arg("push") {
                cli_args.push("--push".to_string());
            }
            sync_command(&cli_args, ctx)
        }
        _ => Err(AxiError::new(
            format!("Unknown MCP tool: {name}"),
            "UNKNOWN_COMMAND",
            vec![],
        )),
    }
}

pub fn tool_definitions() -> Value {
    json!([
        {
            "name": "vault_home",
            "description": "Dashboard: vault stats, recent notes, and next steps.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "vault_path",
            "description": "Resolve a note's absolute path (and create parent folders) or print the vault root.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "file": { "type": "string", "description": "Optional vault-relative note path" }
                }
            }
        },
        {
            "name": "vault_read",
            "description": "Read a note (partial read by outline, section, frontmatter, or line range).",
            "inputSchema": {
                "type": "object",
                "required": ["file"],
                "properties": {
                    "file": { "type": "string", "description": "Vault-relative note path" },
                    "outline": { "type": "boolean", "description": "Return heading structure only" },
                    "section": { "type": "string", "description": "Heading path e.g. 'Design' or 'Design/Sub'" },
                    "frontmatter": { "type": "boolean", "description": "Return YAML frontmatter only" },
                    "lines": { "type": "string", "description": "1-based line range 'a-b'" },
                    "full": { "type": "boolean", "description": "Disable content truncation" }
                }
            }
        },
        {
            "name": "vault_write",
            "description": "Create or overwrite a note with optional title and frontmatter fields in one call.",
            "inputSchema": {
                "type": "object",
                "required": ["file", "content"],
                "properties": {
                    "file": { "type": "string" },
                    "title": { "type": "string" },
                    "content": { "type": "string" },
                    "frontmatter": { "type": "object" },
                    "force": { "type": "boolean" }
                }
            }
        },
        {
            "name": "vault_update",
            "description": "Surgical in-place edit of a note section, frontmatter field, or unique text anchor.",
            "inputSchema": {
                "type": "object",
                "required": ["file"],
                "properties": {
                    "file": { "type": "string" },
                    "section": { "type": "string" },
                    "mode": { "type": "string", "enum": ["replace", "append", "prepend"] },
                    "content": { "type": "string" },
                    "frontmatter": { "type": "object" },
                    "anchor": { "type": "string" },
                    "replace": { "type": "string" }
                }
            }
        },
        {
            "name": "vault_batch",
            "description": "Apply multiple section, anchor, frontmatter, or insert operations atomically.",
            "inputSchema": {
                "type": "object",
                "required": ["file", "ops"],
                "properties": {
                    "file": { "type": "string" },
                    "ops": { "type": "array", "items": { "type": "object" } }
                }
            }
        },
        {
            "name": "vault_insert",
            "description": "Insert a block before/after a heading or at the start/end of a note.",
            "inputSchema": {
                "type": "object",
                "required": ["file", "content"],
                "properties": {
                    "file": { "type": "string" },
                    "after_heading": { "type": "string" },
                    "before_heading": { "type": "string" },
                    "append": { "type": "boolean" },
                    "prepend": { "type": "boolean" },
                    "content": { "type": "string" }
                }
            }
        },
        {
            "name": "vault_log",
            "description": "Append a timestamped build log entry to a task's '## Build Log' section.",
            "inputSchema": {
                "type": "object",
                "required": ["file", "entry"],
                "properties": {
                    "file": { "type": "string", "description": "Task note path (e.g. 'plans/my-task.md')" },
                    "entry": { "type": "string", "description": "Log entry content" },
                    "kind": { "type": "string", "description": "Optional free-form log category/label" },
                    "section": { "type": "string", "description": "Target section heading (defaults to 'Build Log')" }
                }
            }
        },
        {
            "name": "vault_check",
            "description": "List or toggle markdown checklist items (- [ ] / - [x]) by substring match.",
            "inputSchema": {
                "type": "object",
                "required": ["file"],
                "properties": {
                    "file": { "type": "string" },
                    "task": { "type": "string", "description": "Substring of checklist item to toggle" },
                    "uncheck": { "type": "boolean" },
                    "all": { "type": "boolean" },
                    "list": { "type": "boolean" }
                }
            }
        },
        {
            "name": "vault_search",
            "description": "Hybrid FTS5 + semantic search with optional folder, status, tag, and since filters.",
            "inputSchema": {
                "type": "object",
                "required": ["query"],
                "properties": {
                    "query": { "type": "string" },
                    "mode": { "type": "string", "enum": ["hybrid", "fts", "semantic"] },
                    "limit": { "type": "integer" },
                    "folder": { "type": "string" },
                    "status": { "type": "string" },
                    "tag": { "type": "string" },
                    "since": { "type": "string" },
                    "full": { "type": "boolean" }
                }
            }
        },
        {
            "name": "vault_context",
            "description": "Fetch a note along with outlines and previews of its linked/backlinked neighbors in one call.",
            "inputSchema": {
                "type": "object",
                "required": ["file"],
                "properties": {
                    "file": { "type": "string" },
                    "limit": { "type": "integer" },
                    "full": { "type": "boolean" }
                }
            }
        },
        {
            "name": "vault_related",
            "description": "Find notes connected by [[wikilinks]] and semantic similarity.",
            "inputSchema": {
                "type": "object",
                "required": ["file"],
                "properties": {
                    "file": { "type": "string" },
                    "limit": { "type": "integer" }
                }
            }
        },
        {
            "name": "vault_links",
            "description": "List outbound links (flagging dangling ones) and backlinks for a note.",
            "inputSchema": {
                "type": "object",
                "required": ["file"],
                "properties": { "file": { "type": "string" } }
            }
        },
        {
            "name": "vault_ls",
            "description": "List notes with optional directory, status, tag, and since filters.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "dir": { "type": "string" },
                    "status": { "type": "string" },
                    "tag": { "type": "string" },
                    "since": { "type": "string" },
                    "limit": { "type": "integer" }
                }
            }
        },
        {
            "name": "vault_move",
            "description": "Move/rename a note and rewrite all [[wikilinks]] across the vault.",
            "inputSchema": {
                "type": "object",
                "required": ["old_path", "new_path"],
                "properties": {
                    "old_path": { "type": "string" },
                    "new_path": { "type": "string" },
                    "force": { "type": "boolean" }
                }
            }
        },
        {
            "name": "vault_delete",
            "description": "Delete a note and warn if incoming backlinks become dangling.",
            "inputSchema": {
                "type": "object",
                "required": ["file"],
                "properties": { "file": { "type": "string" } }
            }
        },
        {
            "name": "vault_reindex",
            "description": "Refresh the search and link index.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "force": { "type": "boolean" },
                    "no_embed": { "type": "boolean" }
                }
            }
        },
        {
            "name": "vault_doctor",
            "description": "Audit vault health: dangling links, orphan notes, and missing frontmatter.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "vault_sync",
            "description": "Trigger cloud synchronization (Vault Cloud, Git, or S3).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "pull": { "type": "boolean" },
                    "push": { "type": "boolean" }
                }
            }
        }
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_db_at;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static MCP_COUNTER: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn dispatch_mcp_tool_covers_all_tools_and_unknown_tool() {
        std::env::set_var("VAULT_AXI_NO_SYNC", "1");
        std::env::set_var("VAULT_AXI_NO_EMBED", "1");

        let seq = MCP_COUNTER.fetch_add(1, Ordering::SeqCst);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "vault-axi-mcp-test-{}-{}-{}",
            std::process::id(),
            nanos,
            seq
        ));
        fs::create_dir_all(&dir).unwrap();
        let root = dir.to_string_lossy().to_string();
        let db = open_db_at(":memory:").unwrap();
        let mut ctx = Ctx::with_vault_and_db(root.clone(), db);

        // 1. vault_write & vault_home & vault_path
        let w = dispatch_mcp_tool(
            "vault_write",
            &json!({
                "file": "plans/task.md",
                "title": "Task",
                "content": "## Overview\nSee [[other]].\n\n## Tasks\n- [ ] Step 1\n",
                "frontmatter": {"status": "in-progress", "tags": ["rust"]}
            }),
            &mut ctx,
        )
        .unwrap();
        assert_eq!(w["file"], "plans/task.md");
        assert_eq!(w["action"], "created");

        let home = dispatch_mcp_tool("vault_home", &json!({}), &mut ctx).unwrap();
        assert_eq!(home["notes"], 1);

        let p_root = dispatch_mcp_tool("vault_path", &json!({}), &mut ctx).unwrap();
        assert!(p_root["vault"].as_str().unwrap().contains("vault-axi-mcp-test"));
        let p_note =
            dispatch_mcp_tool("vault_path", &json!({"file": "plans/sub/new.md"}), &mut ctx)
                .unwrap();
        assert_eq!(p_note["file"], "plans/sub/new.md");

        // 2. vault_update & vault_insert & vault_batch
        let upd = dispatch_mcp_tool(
            "vault_update",
            &json!({
                "file": "plans/task.md",
                "section": "Overview",
                "mode": "append",
                "content": "Appended overview.",
                "frontmatter": {"owner": "alice"}
            }),
            &mut ctx,
        )
        .unwrap();
        assert_eq!(upd["changed"], true);

        let ins = dispatch_mcp_tool(
            "vault_insert",
            &json!({
                "file": "plans/task.md",
                "before_heading": "Tasks",
                "content": "## Notes\nInserted note."
            }),
            &mut ctx,
        )
        .unwrap();
        assert_eq!(ins["changed"], true);

        let batch = dispatch_mcp_tool(
            "vault_batch",
            &json!({
                "file": "plans/task.md",
                "ops": [
                    {"op": "anchor", "anchor": "Inserted note.", "replace": "Batch replaced note."}
                ]
            }),
            &mut ctx,
        )
        .unwrap();
        assert_eq!(batch["operationsApplied"], 1);

        // 3. vault_log & vault_check
        let lg = dispatch_mcp_tool(
            "vault_log",
            &json!({
                "file": "plans/task.md",
                "entry": "Did work",
                "kind": "progress",
                "section": "Build Log"
            }),
            &mut ctx,
        )
        .unwrap();
        assert_eq!(lg["file"], "plans/task.md");

        let chk_list = dispatch_mcp_tool(
            "vault_check",
            &json!({"file": "plans/task.md", "list": true}),
            &mut ctx,
        )
        .unwrap();
        assert_eq!(chk_list["totalTasks"], 1);

        let chk_toggle = dispatch_mcp_tool(
            "vault_check",
            &json!({"file": "plans/task.md", "task": "Step 1", "all": true}),
            &mut ctx,
        )
        .unwrap();
        assert_eq!(chk_toggle["items"].as_array().unwrap().len(), 1);

        // 4. vault_read & vault_search & vault_ls & vault_links & vault_related & vault_context
        let rd = dispatch_mcp_tool(
            "vault_read",
            &json!({"file": "plans/task.md", "outline": true}),
            &mut ctx,
        )
        .unwrap();
        assert!(!rd["outline"].as_array().unwrap().is_empty());

        let srch = dispatch_mcp_tool(
            "vault_search",
            &json!({
                "query": "Batch replaced",
                "mode": "fts",
                "folder": "plans",
                "status": "in-progress",
                "tag": "rust",
                "since": "7d",
                "limit": 5,
                "full": true
            }),
            &mut ctx,
        )
        .unwrap();
        assert_eq!(srch["results"].as_array().unwrap().len(), 1);

        let ls = dispatch_mcp_tool(
            "vault_ls",
            &json!({
                "dir": "plans",
                "status": "in-progress",
                "tag": "rust",
                "since": "7d",
                "limit": 5
            }),
            &mut ctx,
        )
        .unwrap();
        assert_eq!(ls["total"], 1);

        let lnk =
            dispatch_mcp_tool("vault_links", &json!({"file": "plans/task.md"}), &mut ctx).unwrap();
        assert_eq!(lnk["outboundCount"], 1);

        let rel = dispatch_mcp_tool(
            "vault_related",
            &json!({"file": "plans/task.md", "limit": 5}),
            &mut ctx,
        )
        .unwrap();
        assert_eq!(rel["file"], "plans/task.md");

        let c_res = dispatch_mcp_tool(
            "vault_context",
            &json!({"file": "plans/task.md", "limit": 5, "full": true}),
            &mut ctx,
        )
        .unwrap();
        assert_eq!(c_res["file"], "plans/task.md");

        // 5. vault_reindex & vault_doctor & vault_move & vault_delete & unknown
        let reidx = dispatch_mcp_tool(
            "vault_reindex",
            &json!({"force": true, "no_embed": true}),
            &mut ctx,
        )
        .unwrap();
        assert_eq!(reidx["scanned"], 1);

        let doc = dispatch_mcp_tool("vault_doctor", &json!({}), &mut ctx).unwrap();
        assert_eq!(doc["totalNotes"], 1);

        let mv = dispatch_mcp_tool(
            "vault_move",
            &json!({"old_path": "plans/task.md", "new_path": "plans/moved.md", "force": true}),
            &mut ctx,
        )
        .unwrap();
        assert_eq!(mv["to"], "plans/moved.md");

        let del = dispatch_mcp_tool(
            "vault_delete",
            &json!({"file": "plans/moved.md"}),
            &mut ctx,
        )
        .unwrap();
        assert_eq!(del["deleted"], true);

        let unknown = dispatch_mcp_tool("vault_nonexistent", &json!({}), &mut ctx).unwrap_err();
        assert_eq!(unknown.code, "UNKNOWN_COMMAND");

        let _ = fs::remove_dir_all(&dir);
    }
}
