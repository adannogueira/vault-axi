#!/usr/bin/env node
import { runAxiCli } from "axi-sdk-js";
import { createContext, type Ctx } from "./context.js";
import { homeCommand } from "./commands/home.js";
import { readCommand } from "./commands/read.js";
import { writeCommand } from "./commands/write.js";
import { updateCommand } from "./commands/update.js";
import { insertCommand } from "./commands/insert.js";
import { reindexCommand } from "./commands/reindex.js";
import { searchCommand } from "./commands/search.js";
import { relatedCommand } from "./commands/related.js";
import { linksCommand } from "./commands/links.js";
import { lsCommand } from "./commands/ls.js";
import { moveCommand } from "./commands/move.js";
import { deleteCommand } from "./commands/delete.js";
import { setupCommand } from "./commands/setup.js";

const DESCRIPTION = "Read, edit, and search an Obsidian markdown vault — built for agents.";

const TOP_LEVEL_HELP = `${DESCRIPTION}

Usage: vault <command> [args] [flags]

Commands:
  (none)                 Dashboard: vault stats, recent notes, next steps
  read <file>            Read a note; --outline | --section <h> | --frontmatter | --lines a-b | --full
  write <file>           Create a note; --content "..." | --content - (stdin); --force to overwrite
  update <file>          Surgical edit; --section <h> --mode replace|append|prepend | --frontmatter k=v | --anchor/--replace
  insert <file>          Insert a block; --after-heading <h> | --before-heading <h> | --append | --prepend
  search <query>         Hybrid FTS5 + semantic search; --mode hybrid|fts|semantic | --limit N | --folder <dir>
  related <file>         Notes related by [[links]] and meaning
  links <file>           Outbound links and backlinks
  ls [dir]               List notes with status/tags; --limit N
  move <old> <new>       Move/rename a note and rewrite [[wikilinks]] vault-wide
  delete <file>          Delete a note (warns about incoming backlinks)
  reindex                Refresh the search index; --force | --no-embed
  setup                  Configure the vault path and install the session hook; --vault <path> | hooks

Flags for content: pass --content "text", or --content - / --stdin to read from stdin.
`;

const COMMAND_HELP: Record<string, string> = {
  read: `vault read <file> [flags]
  --outline            List heading structure only
  --section "<h>"      One section by heading path (e.g. "Design" or "Design/Sub-detail")
  --frontmatter        Just the YAML frontmatter
  --lines a-b          A line range
  --full               Defeat truncation
Examples:
  vault read plans/foo.md --outline
  vault read plans/foo.md --section "Open Questions"
`,
  write: `vault write <file> --content "..." [--force]
  --content "text"     Note body (use --content - or --stdin to read stdin)
  --force              Overwrite if the note exists
Example:
  vault write drafts/idea.md --content "# Idea\\n\\nnotes"
`,
  update: `vault update <file> <mode>
  --section "<h>" --mode replace|append|prepend --content "..."
  --frontmatter key=value        (repeatable; omit value to unset)
  --anchor "exact" --replace "new text"
Examples:
  vault update plans/foo.md --section "Design" --mode replace --content "new body"
  vault update plans/foo.md --frontmatter status=active
  vault update plans/foo.md --anchor "old sentence." --replace "new sentence."
`,
  insert: `vault insert <file> <where> --content "..."
  --after-heading "<h>" | --before-heading "<h>" | --append | --prepend
Example:
  vault insert plans/foo.md --after-heading "Goals" --content "- new goal"
`,
  search: `vault search "<query>" [flags]
  --mode hybrid|fts|semantic   Default hybrid (RRF fusion of keyword + meaning)
  --limit N                    Max results (default 8)
  --folder <dir>               Restrict to a folder
  --full                       Show full block text, not a preview
Example:
  vault search "targeted alerts for metrics service"
`,
  related: `vault related <file> [--limit N]
Ranks notes connected by [[links]]/backlinks first, then by semantic similarity.
`,
  links: `vault links <file>
Lists outbound links (with dangling ones flagged) and backlinks.
`,
  ls: `vault ls [dir] [--limit N]
Lists notes with their frontmatter status and tags.
`,
  move: `vault move <old> <new> [--force]
Moves/renames a note and rewrites every [[wikilink]] that pointed at it.
`,
  delete: `vault delete <file>
Deletes a note (idempotent) and warns which notes now have dangling links.
`,
  reindex: `vault reindex [--force] [--no-embed]
  --force     Reparse every note regardless of mtime
  --no-embed  Skip embeddings (FTS5 + graph only)
`,
  setup: `vault setup [--vault <path>] [hooks]
  --vault <path>   Point at your Obsidian vault (saved to ~/.vault-axi/config.json)
  hooks            Install the Claude Code SessionStart hook so the dashboard loads each session
Examples:
  vault setup --vault ~/Documents/obsidian-vault/Vault
  vault setup hooks
`,
};

await runAxiCli<Ctx>({
  description: DESCRIPTION,
  version: "0.1.0",
  topLevelHelp: TOP_LEVEL_HELP,
  resolveContext: () => createContext(),
  getCommandHelp: (command) => COMMAND_HELP[command] ?? null,
  home: homeCommand,
  commands: {
    read: readCommand,
    write: writeCommand,
    update: updateCommand,
    insert: insertCommand,
    search: searchCommand,
    related: relatedCommand,
    links: linksCommand,
    ls: lsCommand,
    move: moveCommand,
    delete: deleteCommand,
    reindex: reindexCommand,
    setup: setupCommand,
  },
});
