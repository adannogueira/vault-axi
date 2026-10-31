pub mod args;
pub mod axi;
pub mod commands;
pub mod config;
pub mod context;
pub mod db;
pub mod index;
pub mod markdown;
pub mod render;
pub mod sync;
pub mod vault;

use crate::axi::{print_error, print_toon, AxiError};
use crate::commands::{
    batch::batch_command, check::check_command, context_cmd::context_command,
    delete::delete_command, doctor::doctor_command, home::home_command, insert::insert_command,
    links::links_command, log::log_command, ls::ls_command, mcp::mcp_command,
    move_cmd::move_command, path::path_command, read::read_command, reindex::reindex_command,
    related::related_command, search::search_command, setup::setup_command,
    sync_cmd::{login_command, register_command, sync_command},
    update::update_command, write::write_command,
};
use crate::context::Ctx;
use std::io::{self, Write};

const VERSION: &str = "0.1.0";
const DESCRIPTION: &str = "Read, edit, and search an Obsidian markdown vault — built for agents.";

const TOP_LEVEL_HELP: &str = r#"Read, edit, and search an Obsidian markdown vault — built for agents.

Usage: vault <command> [args] [flags]

Commands:
  (none)                 Dashboard: vault stats, recent notes, next steps
  read <file>            Read a note; --outline | --section <h> | --frontmatter | --lines a-b | --full
  context <file>         1-call context bundle: note + outlines/previews of connected [[wikilinks]]
  path [note]            Resolve a note's absolute path, or print the vault root
  write <file>           Create a note; [--title "..."] [--frontmatter k=v] (--from <file> | --content "..." | --stdin)
  update <file>          Surgical edit; --section <h> --mode replace|append|prepend | --frontmatter k=v | --anchor/--replace
  batch <file>           Multi-edit atomic batch; --ops '[...]' | --from <ops.json> | --stdin
  insert <file>          Insert a block; --after-heading <h> | --before-heading <h> | --append | --prepend
  log <file> "<entry>"   Append timestamped entry to a task's ## Build Log; [--kind <label>] [--section <h>]
  check <file> [query]   Toggle (- [x] / - [ ]) or --list markdown checklist items in place
  search <query>         Hybrid FTS5 + semantic search; --mode hybrid|fts|semantic | --status <s> | --tag <t> | --since 7d
  related <file>         Notes related by [[links]] and meaning
  links <file>           Outbound links and backlinks
  ls [dir]               List notes with status/tags; --status <s> | --tag <t> | --since 7d | --limit N
  doctor                 Audit vault health: dangling links, orphan notes, and missing frontmatter
  move <old> <new>       Move/rename a note and rewrite [[wikilinks]] vault-wide
  delete <file>          Delete a note (warns about incoming backlinks)
  reindex                Refresh the search index; --force | --no-embed
  register               Create an account on Vault Cloud (--email <e> --password <p>)
  login                  Sign in to Vault Cloud (--email <e> --password <p> [--mfa-code <code>])
  sync                   Synchronize with cloud backend (Vault Cloud, Git, or S3); --pull | --push
  setup                  Configure vault path, remote backend (git/s3), or 'hooks'
  mcp                    Run built-in Model Context Protocol (MCP) server over stdio"#;

fn get_command_help(command: &str) -> Option<&'static str> {
    match command {
        "read" => Some(
            r#"vault read <file> [flags]
  --outline            List heading structure only
  --section "<h>"      One section by heading path (e.g. "Design" or "Design/Sub-detail")
  --frontmatter        Just the YAML frontmatter
  --lines a-b          A line range
  --full               Defeat truncation"#,
        ),
        "context" => Some(
            r#"vault context <file> [--limit N] [--full]
Returns the note's frontmatter, outline, and body alongside outlines and previews of its
outbound [[wikilinks]] and incoming backlinks in a single call."#,
        ),
        "write" => Some(
            r#"vault write <file> [--title "..."] [--frontmatter k=v] (--from <src> | --content "..." | --stdin) [--force]
  --title "<title>"      Prepend an H1 heading (# <title>) if not already in content
  --frontmatter k=v      Set initial YAML frontmatter field(s) (repeatable)
  --from <src>           Use the contents of <src> as the note body
  --content "text"       Inline body (use --content - or --stdin to read stdin)
  --force                Overwrite if the note exists"#,
        ),
        "path" => Some(
            r#"vault path [note]
  (no arg)   Print the vault root folder
  <note>     Resolve the note's absolute path and create its parent folders."#,
        ),
        "update" => Some(
            r#"vault update <file> <mode>
  --section "<h>" --mode replace|append|prepend (--from <src> | --content "...")
  --frontmatter key=value        (repeatable; omit value to unset)
  --anchor "exact" --replace "new text""#,
        ),
        "batch" => Some(
            r#"vault batch <file> (--ops '<json-array>' | --from <ops.json> | --stdin)
Applies multiple surgical edits atomically in one call. Supported op types:
  {"op":"section","heading":"Design","mode":"replace|append|prepend","content":"..."}
  {"op":"anchor","old_str":"exact old","new_str":"new text"}
  {"op":"frontmatter","key":"status","value":"active"}
  {"op":"insert","where_anchor":"after-heading|before-heading|append|prepend","heading":"Goals","content":"..."}"#,
        ),
        "insert" => Some(
            r#"vault insert <file> <where> (--from <src> | --content "...")
  --after-heading "<h>" | --before-heading "<h>" | --append | --prepend"#,
        ),
        "log" => Some(
            r#"vault log <file> "<entry>" [--kind <label>] [--section "Build Log"]
Appends a timestamped entry (`- YYYY-MM-DD HH:MM — **[<KIND>]** <entry>`) directly into <file>'s
`## Build Log` section (creating the section if absent). `--kind` accepts any free-form string label.
Requires `<file>` on every call so multiple concurrent agent sessions in different tabs never collide.
Examples:
  vault log plans/metrics-alerts.md "Switched to NRQL count query because Apdex was noisy" --kind decision
  vault log plans/metrics-alerts.md "Found trailing colon mismatch in prod" --kind trap"#,
        ),
        "check" => Some(
            r#"vault check <file> ["<task substring>"] [--uncheck] [--all] [--list]
  --list       List all markdown checklist items (- [ ] / - [x]) with line numbers
  --uncheck    Mark matching item(s) as unchecked (- [ ]) instead of checked (- [x])
  --all        Toggle all matching items if substring matches more than one"#,
        ),
        "search" => Some(
            r#"vault search "<query>" [flags]
  --mode hybrid|fts|semantic   Default hybrid (RRF fusion of keyword + meaning)
  --limit N                    Max results (default 8)
  --folder <dir>               Restrict to a folder
  --status <status>            Filter by frontmatter status
  --tag <tag>                  Filter by frontmatter tag
  --since <7d|24h|YYYY-MM-DD>  Filter by modification time
  --full                       Show full block text, not a preview"#,
        ),
        "related" => Some(
            r#"vault related <file> [--limit N]
Ranks notes connected by [[links]]/backlinks first, then by semantic similarity."#,
        ),
        "links" => Some(
            r#"vault links <file>
Lists outbound links (with dangling ones flagged) and backlinks."#,
        ),
        "ls" => Some(
            r#"vault ls [dir] [--status <s>] [--tag <t>] [--since <7d|YYYY-MM-DD>] [--limit N]
Lists notes with their frontmatter status and tags."#,
        ),
        "doctor" => Some(
            r#"vault doctor
Audits vault health: lists all dangling [[wikilinks]], orphan notes, and notes missing YAML frontmatter."#,
        ),
        "move" => Some(
            r#"vault move <old> <new> [--force]
Moves/renames a note and rewrites every [[wikilink]] that pointed at it."#,
        ),
        "delete" => Some(
            r#"vault delete <file>
Deletes a note (idempotent) and warns which notes now have dangling links."#,
        ),
        "reindex" => Some(
            r#"vault reindex [--force] [--no-embed]
  --force     Reparse every note regardless of mtime
  --no-embed  Skip embeddings (FTS5 + graph only)"#,
        ),
        "register" => Some(
            r#"vault register --email <you@example.com> --password <password> [--vault-id <id>]
Creates a new account on Vault Cloud (hosted multi-tenant cloud with strict per-user RLS isolation)
and automatically configures local sync."#,
        ),
        "login" => Some(
            r#"vault login --email <you@example.com> --password <password> [--mfa-code <6-digit-totp>] [--vault-id <id>]
  --mfa-enroll          Enroll a new TOTP authenticator app for Multi-Factor Authentication
  --mfa-verify <code>   Verify and activate TOTP MFA (elevates session to AAL2)
Signs in to Vault Cloud and enables automatic background delta sync."#,
        ),
        "sync" => Some(
            r#"vault sync [--pull] [--push]
Synchronizes notes with the configured cloud backend (Hosted Cloud, Git, or S3/R2)."#,
        ),
        "setup" => Some(
            r#"vault setup [--vault <path>] [--remote <git-url|s3://...>] [--auto-sync true|false] [hooks]
  --vault <path>            Point at your local Obsidian vault
  --remote git:<repo-url>   Configure Git cloud backend
  --remote s3://<bucket>    Configure S3/R2 cloud backend (with --endpoint <https://...>)
  hooks                     Install Claude Code SessionStart dashboard hook + skill"#,
        ),
        "mcp" => Some(
            r#"vault mcp
Starts a built-in Model Context Protocol (MCP) JSON-RPC 2.0 server over stdio.
Add `"command": "vault", "args": ["mcp"]` to Claude Desktop or Cursor MCP settings."#,
        ),
        _ => None,
    }
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();

    if argv.is_empty() {
        let mut ctx = Ctx::new();
        match home_command(&[], &mut ctx) {
            Ok(val) => {
                print_toon(&val);
                std::process::exit(0);
            }
            Err(err) => {
                print_error(&err);
                std::process::exit(err.exit_code());
            }
        }
    }

    let first = argv[0].as_str();
    if first == "-v" || first == "--version" || first == "version" {
        let _ = writeln!(io::stdout(), "{VERSION}");
        std::process::exit(0);
    }
    if first == "-h" || first == "--help" || first == "help" {
        if let Some(cmd) = argv.get(1) {
            if let Some(help) = get_command_help(cmd) {
                let _ = writeln!(io::stdout(), "{help}");
                std::process::exit(0);
            }
        }
        let _ = writeln!(io::stdout(), "{TOP_LEVEL_HELP}");
        std::process::exit(0);
    }

    let rest = &argv[1..];
    if rest.iter().any(|a| a == "-h" || a == "--help") {
        if let Some(help) = get_command_help(first) {
            let _ = writeln!(io::stdout(), "{help}");
            std::process::exit(0);
        } else {
            let _ = writeln!(io::stdout(), "{TOP_LEVEL_HELP}");
            std::process::exit(0);
        }
    }

    if first == "mcp" {
        if let Err(err) = mcp_command() {
            print_error(&err);
            std::process::exit(err.exit_code());
        }
        std::process::exit(0);
    }

    let mut ctx = Ctx::new();
    let res = match first {
        "read" => read_command(rest, &mut ctx),
        "context" => context_command(rest, &mut ctx),
        "path" => path_command(rest, &mut ctx),
        "write" => write_command(rest, &mut ctx),
        "update" => update_command(rest, &mut ctx),
        "batch" => batch_command(rest, &mut ctx),
        "insert" => insert_command(rest, &mut ctx),
        "log" => log_command(rest, &mut ctx),
        "check" => check_command(rest, &mut ctx),
        "search" => search_command(rest, &mut ctx),
        "related" => related_command(rest, &mut ctx),
        "links" => links_command(rest, &mut ctx),
        "ls" => ls_command(rest, &mut ctx),
        "doctor" => doctor_command(rest, &mut ctx),
        "move" => move_command(rest, &mut ctx),
        "delete" => delete_command(rest, &mut ctx),
        "reindex" => reindex_command(rest, &mut ctx),
        "register" => register_command(rest, &mut ctx),
        "login" => login_command(rest, &mut ctx),
        "sync" => sync_command(rest, &mut ctx),
        "setup" => setup_command(rest, &mut ctx),
        _ => {
            let _ = DESCRIPTION;
            Err(AxiError::new(
                format!("Unknown command: {first}"),
                "UNKNOWN_COMMAND",
                vec!["Run `vault --help` to see available commands".to_string()],
            ))
        }
    };

    match res {
        Ok(val) => {
            print_toon(&val);
            std::process::exit(0);
        }
        Err(err) => {
            print_error(&err);
            std::process::exit(err.exit_code());
        }
    }
}
