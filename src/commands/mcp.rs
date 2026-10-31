//! Built-in Model Context Protocol (MCP) server over stdio.
//! Allows non-technical or desktop users to connect Claude Desktop, Cursor, or
//! any MCP client directly to the single `vault` binary (`vault mcp`) with zero Node/Python setup.

use crate::axi::AxiError;
use crate::commands::{
    batch::batch_command, check::check_command, context_cmd::context_command,
    delete::delete_command, doctor::doctor_command, home::home_command, insert::insert_command,
    links::links_command, log::log_command, ls::ls_command, move_cmd::move_command,
    read::read_command, related::related_command, search::search_command, sync_cmd::sync_command,
    update::update_command, write::write_command,
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

fn dispatch_mcp_tool(name: &str, args: &Value, ctx: &mut Ctx) -> Result<Value, AxiError> {
    let mut cli_args: Vec<String> = Vec::new();
    let str_arg = |k: &str| args.get(k).and_then(|v| v.as_str()).map(|s| s.to_string());
    let bool_arg = |k: &str| args.get(k).and_then(|v| v.as_bool()).unwrap_or(false);
    let int_arg = |k: &str| args.get(k).and_then(|v| v.as_i64()).map(|n| n.to_string());

    match name {
        "vault_home" => home_command(&[], ctx),
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
                cli_args.push("--section".to_string());
                cli_args.push(s);
            }
            if let Some(l) = str_arg("lines") {
                cli_args.push("--lines".to_string());
                cli_args.push(l);
            }
            read_command(&cli_args, ctx)
        }
        "vault_write" => {
            if let Some(f) = str_arg("file") {
                cli_args.push(f);
            }
            if let Some(t) = str_arg("title") {
                cli_args.push("--title".to_string());
                cli_args.push(t);
            }
            if let Some(c) = str_arg("content") {
                cli_args.push("--content".to_string());
                cli_args.push(c);
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
                    cli_args.push("--frontmatter".to_string());
                    cli_args.push(format!("{k}={v_str}"));
                }
            }
            write_command(&cli_args, ctx)
        }
        "vault_update" => {
            if let Some(f) = str_arg("file") {
                cli_args.push(f);
            }
            if let Some(s) = str_arg("section") {
                cli_args.push("--section".to_string());
                cli_args.push(s);
            }
            if let Some(m) = str_arg("mode") {
                cli_args.push("--mode".to_string());
                cli_args.push(m);
            }
            if let Some(c) = str_arg("content") {
                cli_args.push("--content".to_string());
                cli_args.push(c);
            }
            if let Some(a) = str_arg("anchor") {
                cli_args.push("--anchor".to_string());
                cli_args.push(a);
            }
            if let Some(r) = str_arg("replace") {
                cli_args.push("--replace".to_string());
                cli_args.push(r);
            }
            if let Some(Value::Object(fm)) = args.get("frontmatter") {
                for (k, v) in fm {
                    cli_args.push("--frontmatter".to_string());
                    if v.is_null() {
                        cli_args.push(k.clone());
                    } else if let Value::String(s) = v {
                        cli_args.push(format!("{k}={s}"));
                    } else {
                        cli_args.push(format!("{k}={v}"));
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
                cli_args.push("--ops".to_string());
                cli_args.push(ops.to_string());
            }
            batch_command(&cli_args, ctx)
        }
        "vault_insert" => {
            if let Some(f) = str_arg("file") {
                cli_args.push(f);
            }
            if let Some(ah) = str_arg("after_heading") {
                cli_args.push("--after-heading".to_string());
                cli_args.push(ah);
            }
            if let Some(bh) = str_arg("before_heading") {
                cli_args.push("--before-heading".to_string());
                cli_args.push(bh);
            }
            if bool_arg("append") {
                cli_args.push("--append".to_string());
            }
            if bool_arg("prepend") {
                cli_args.push("--prepend".to_string());
            }
            if let Some(c) = str_arg("content") {
                cli_args.push("--content".to_string());
                cli_args.push(c);
            }
            insert_command(&cli_args, ctx)
        }
        "vault_log" => {
            if let Some(f) = str_arg("file") {
                cli_args.push(f);
            }
            if let Some(e) = str_arg("entry") {
                cli_args.push("--content".to_string());
                cli_args.push(e);
            }
            if let Some(k) = str_arg("kind") {
                cli_args.push("--kind".to_string());
                cli_args.push(k);
            }
            if let Some(s) = str_arg("section") {
                cli_args.push("--section".to_string());
                cli_args.push(s);
            }
            log_command(&cli_args, ctx)
        }
        "vault_check" => {
            if let Some(f) = str_arg("file") {
                cli_args.push(f);
            }
            if let Some(t) = str_arg("task") {
                cli_args.push(t);
            }
            if bool_arg("uncheck") {
                cli_args.push("--uncheck".to_string());
            }
            if bool_arg("list") {
                cli_args.push("--list".to_string());
            }
            check_command(&cli_args, ctx)
        }
        "vault_search" => {
            if let Some(q) = str_arg("query") {
                cli_args.push(q);
            }
            if let Some(m) = str_arg("mode") {
                cli_args.push("--mode".to_string());
                cli_args.push(m);
            }
            if let Some(l) = int_arg("limit") {
                cli_args.push("--limit".to_string());
                cli_args.push(l);
            }
            if let Some(f) = str_arg("folder") {
                cli_args.push("--folder".to_string());
                cli_args.push(f);
            }
            if let Some(st) = str_arg("status") {
                cli_args.push("--status".to_string());
                cli_args.push(st);
            }
            if let Some(tg) = str_arg("tag") {
                cli_args.push("--tag".to_string());
                cli_args.push(tg);
            }
            search_command(&cli_args, ctx)
        }
        "vault_context" => {
            if let Some(f) = str_arg("file") {
                cli_args.push(f);
            }
            if let Some(l) = int_arg("limit") {
                cli_args.push("--limit".to_string());
                cli_args.push(l);
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
                cli_args.push("--limit".to_string());
                cli_args.push(l);
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
                cli_args.push("--status".to_string());
                cli_args.push(st);
            }
            if let Some(tg) = str_arg("tag") {
                cli_args.push("--tag".to_string());
                cli_args.push(tg);
            }
            if let Some(l) = int_arg("limit") {
                cli_args.push("--limit".to_string());
                cli_args.push(l);
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
        "vault_doctor" => doctor_command(&[], ctx),
        "vault_sync" => sync_command(&[], ctx),
        _ => Err(AxiError::new(
            format!("Unknown MCP tool: {name}"),
            "UNKNOWN_COMMAND",
            vec![],
        )),
    }
}

fn tool_definitions() -> Value {
    json!([
        {
            "name": "vault_home",
            "description": "Dashboard: vault stats, recent notes, and next steps.",
            "inputSchema": { "type": "object", "properties": {} }
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
                    "list": { "type": "boolean" }
                }
            }
        },
        {
            "name": "vault_search",
            "description": "Hybrid FTS5 + semantic search with optional folder, status, and tag filters.",
            "inputSchema": {
                "type": "object",
                "required": ["query"],
                "properties": {
                    "query": { "type": "string" },
                    "mode": { "type": "string", "enum": ["hybrid", "fts", "semantic"] },
                    "limit": { "type": "integer" },
                    "folder": { "type": "string" },
                    "status": { "type": "string" },
                    "tag": { "type": "string" }
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
            "description": "List notes with optional directory, status, and tag filters.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "dir": { "type": "string" },
                    "status": { "type": "string" },
                    "tag": { "type": "string" },
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
            "name": "vault_doctor",
            "description": "Audit vault health: dangling links, orphan notes, and missing frontmatter.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "vault_sync",
            "description": "Trigger cloud synchronization (Hosted Supabase/Turso, Git, or S3).",
            "inputSchema": { "type": "object", "properties": {} }
        }
    ])
}
