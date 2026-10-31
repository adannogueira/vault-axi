pub mod args;
pub mod axi;
pub mod commands;
pub mod config;
pub mod context;
pub mod db;
pub mod index;
pub mod markdown;
pub mod render;
pub mod vault;

use crate::axi::{print_error, print_toon, AxiError};
use crate::commands::{
    delete::delete_command, home::home_command, insert::insert_command, links::links_command,
    ls::ls_command, move_cmd::move_command, path::path_command, read::read_command,
    reindex::reindex_command, related::related_command, search::search_command,
    setup::setup_command, update::update_command, write::write_command,
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
  path [note]            Resolve a note's absolute path (write to it directly), or print the vault root
  write <file>           Create a note; --from <file> | --content "..." | stdin; --force to overwrite
  update <file>          Surgical edit; --section <h> --mode replace|append|prepend | --frontmatter k=v | --anchor/--replace
  insert <file>          Insert a block; --after-heading <h> | --before-heading <h> | --append | --prepend
  search <query>         Hybrid FTS5 + semantic search; --mode hybrid|fts|semantic | --limit N | --folder <dir>
  related <file>         Notes related by [[links]] and meaning
  links <file>           Outbound links and backlinks
  ls [dir]               List notes with status/tags; --limit N
  move <old> <new>       Move/rename a note and rewrite [[wikilinks]] vault-wide
  delete <file>          Delete a note (warns about incoming backlinks)
  reindex                Refresh the search index; --force | --no-embed
  setup                  Configure the vault path; 'hooks' installs the session dashboard hook + skill

Providing content: for long notes prefer writing the file directly (see `vault path`) or
--from <file>. --content "text" and --content - / --stdin also work for short content."#;

fn get_command_help(command: &str) -> Option<&'static str> {
    match command {
        "read" => Some(
            r#"vault read <file> [flags]
  --outline            List heading structure only
  --section "<h>"      One section by heading path (e.g. "Design" or "Design/Sub-detail")
  --frontmatter        Just the YAML frontmatter
  --lines a-b          A line range
  --full               Defeat truncation
Examples:
  vault read plans/foo.md --outline
  vault read plans/foo.md --section "Open Questions""#,
        ),
        "write" => Some(
            r#"vault write <file> (--from <src> | --content "..." | --stdin) [--force]
  --from <src>         Use the contents of <src> as the note body (best for long notes)
  --content "text"     Inline body (use --content - or --stdin to read stdin)
  --force              Overwrite if the note exists
Tip: for a brand-new long note, write it straight into the vault with your file tool —
     run `vault path <file>` to get the destination path; it's indexed automatically.
Examples:
  vault path drafts/idea.md          # then write your Markdown to the printed path
  vault write drafts/idea.md --from /tmp/idea.md"#,
        ),
        "path" => Some(
            r#"vault path [note]
  (no arg)   Print the vault root folder
  <note>     Resolve the note's absolute path and create its parent folders, so you can
             write the note directly with your own file tool (no scratch file, no piping).
             The vault indexes the file automatically on the next read/search.
Example:
  vault path decisions/use-fts5.md"#,
        ),
        "update" => Some(
            r#"vault update <file> <mode>
  --section "<h>" --mode replace|append|prepend (--from <src> | --content "...")
  --frontmatter key=value        (repeatable; omit value to unset)
  --anchor "exact" --replace "new text"
  (--from <src> reads the new body from a file; best for long section bodies)
Examples:
  vault update plans/foo.md --section "Design" --mode replace --content "new body"
  vault update plans/foo.md --frontmatter status=active
  vault update plans/foo.md --anchor "old sentence." --replace "new sentence.""#,
        ),
        "insert" => Some(
            r#"vault insert <file> <where> (--from <src> | --content "...")
  --after-heading "<h>" | --before-heading "<h>" | --append | --prepend
Example:
  vault insert plans/foo.md --after-heading "Goals" --content "- new goal""#,
        ),
        "search" => Some(
            r#"vault search "<query>" [flags]
  --mode hybrid|fts|semantic   Default hybrid (RRF fusion of keyword + meaning)
  --limit N                    Max results (default 8)
  --folder <dir>               Restrict to a folder
  --full                       Show full block text, not a preview
Example:
  vault search "targeted alerts for metrics service""#,
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
            r#"vault ls [dir] [--limit N]
Lists notes with their frontmatter status and tags."#,
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
        "setup" => Some(
            r#"vault setup [--vault <path>] [hooks]
  --vault <path>   Point at your Obsidian vault (saved to ~/.vault-axi/config.json)
  hooks            Wire vault into agent sessions: install the Claude Code SessionStart
                   dashboard hook AND the `vault` skill (~/.claude/skills/vault)
Examples:
  vault setup --vault ~/Documents/obsidian-vault/Vault
  vault setup hooks"#,
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

    let mut ctx = Ctx::new();
    let res = match first {
        "read" => read_command(rest, &mut ctx),
        "path" => path_command(rest, &mut ctx),
        "write" => write_command(rest, &mut ctx),
        "update" => update_command(rest, &mut ctx),
        "insert" => insert_command(rest, &mut ctx),
        "search" => search_command(rest, &mut ctx),
        "related" => related_command(rest, &mut ctx),
        "links" => links_command(rest, &mut ctx),
        "ls" => ls_command(rest, &mut ctx),
        "move" => move_command(rest, &mut ctx),
        "delete" => delete_command(rest, &mut ctx),
        "reindex" => reindex_command(rest, &mut ctx),
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
