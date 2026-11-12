---
name: vault
description: Read, edit, and search an Obsidian markdown vault via the `vault` CLI. Use for any note/knowledge-base operation - reading a section or frontmatter, surgical mid-document edits, task build logs, semantic + keyword search, following [[wikilinks]], and moving/renaming notes with link rewriting.
---

# vault - Obsidian vault as second brain

`vault` is an agent-ergonomic CLI over an Obsidian vault folder (with automatic cloud sync via Vault Cloud, Git, or S3 / Cloudflare R2). Files are the source of truth; edits are surgical (no full-file rewrites); links use `[[wikilink]]` syntax so the vault stays navigable in Obsidian. It works with Obsidian closed.

Run `vault` (no args) for a live dashboard, or `vault <command> --help` for any command.

## When to use

Reach for `vault` whenever you'd read from or write to the knowledge base: recalling past decisions/research/debugging notes, saving a spec or brainstorm, logging build decisions/deviations during a task, updating a plan, or exploring how notes connect.

## Reading & Context Gathering (partial reads - don't slurp whole files)

```
vault read <file>                     # truncated body (1200 chars) + char count
vault read <file> --outline           # heading structure only
vault read <file> --section "Design"  # one section (supports "Parent/Child" or "Parent::Child" paths)
vault read <file> --frontmatter       # just the YAML frontmatter
vault read <file> --lines 40-80       # a 1-based inclusive line range
vault read <file> --full              # defeat truncation (works with whole note or --section)
vault context <file> [--limit 6] [--full] # 1-call bundle: note + outlines/previews of connected [[wikilinks]]
vault path [note]                     # print vault root (no arg) or resolve <note>'s absolute path & create parent dirs
```

## Creating & Editing Notes (100% self-contained in `vault`)

You do NOT need external file tools or MCPs to create or edit notes:

```
# Fast-capture a thought or snippet into today's daily note (daily/YYYY-MM-DD.md under ## Captures by default):
vault capture "Quick thought or finding" [--to inbox/ideas.md] [--section "Captures"] [--tag work]

# Create a note with title, frontmatter, and body in one call (pass --force to overwrite existing):
vault write plans/my-plan.md --title "My Plan" --frontmatter status=draft --frontmatter tags="[axi, rust]" --from body.md
vault write drafts/quick.md --title "Quick Idea" --content "Initial body" [--force] [--stdin]

# Surgical section / frontmatter / anchor updates:
vault update <file> --section "Design" --mode replace --from body.md # mode: replace (default) | append | prepend
vault update <file> --section "Goals" --mode append --content "…"    # append or prepend within a section
vault update <file> --frontmatter status=active                      # set a frontmatter field (repeatable; omit value to unset)
vault update <file> --anchor "exact old text" --replace "new text"   # unique-match text swap

# Multi-operation atomic batch edits (--ops '<json>' | --from <ops.json> | --stdin):
# Supported ops: section (replace|append|prepend), anchor (old_str/new_str), frontmatter (key/value), insert (after-heading|before-heading|append|prepend)
vault batch <file> --ops '[{"op":"section","heading":"Design","mode":"replace","content":"New design"},{"op":"frontmatter","key":"status","value":"active"}]'

# Insert blocks, attach files/images (with local OCR indexing), or toggle checklist items:
vault insert <file> --after-heading "Goals" --content "- new item"   # or --before-heading "<h>"
vault insert <file> --append --from more.md                          # --append or --prepend to the doc
vault attach <note> <file_path> [--folder assets] [--section "<h>"]  # copy image/doc into vault, embed ![[...]], and index image OCR
vault attachments [<note>]                                           # list attachments in vault or linked from <note>
vault check <file> "Acceptance criterion substring" [--all]          # check (- [x]) or --uncheck (- [ ]) matching task(s) in place
vault check <file> [--list]                                          # list all checklist items with 1-based line numbers
```

## Task Build Logs (`vault log`) & Edit History (`vault history`)

While implementing a task that has a corresponding vault file, log entries with automatic timestamps into the task's `## Build Log` section (`--kind <label>` accepts any free-form string; `--section "<h>"` overrides the target heading; creates the note/section if absent). Every note mutation also records an encrypted version snapshot in `vault history` without polluting search results:

```
vault log plans/my-task.md "Chose bundled SQLite FTS5 for zero-dependency builds" --kind decision
vault log plans/my-task.md "Found Windows file lock when running cargo test + build" --kind trap
vault history <file>                                  # list saved versions of <file>
vault history <file> --version 2 [--full]             # view content of version 2
vault history <file> --diff 2                         # diff version 2 against current note
vault history <file> --restore 2                      # restore <file> to version 2
```

## Searching, Filtering & Navigating

```
vault search "<query>" [--limit 8] [--full]            # hybrid: FTS5 keyword + semantic (RRF-fused; includes image OCR)
vault search "<query>" --folder plans --status active --tag infra --since 7d # filter by folder/status/tag or --since <7d|24h|2w|YYYY-MM-DD>
vault search "<query>" --mode semantic                 # meaning only; --mode fts for keyword only
vault ls [dir] [--status active] [--tag axi] [--since 7d] [--limit 50] # list notes with metadata filters
vault related <file> [--limit 10]                      # notes connected by [[links]] and by meaning
vault links <file>                                     # outbound links (danglers flagged) + backlinks
vault doctor                                           # audit dangling links, orphans, and missing frontmatter
```

## Organizing, Trash, Setup & Cloud Sync

```
vault move <old> <new> [--force]          # rename/move + rewrite every [[wikilink]] that pointed at it
vault delete <file> [--permanent]         # move note/attachment to 7-day trash (or delete permanently) and warn on backlinks
vault trash [--purge]                     # list items recoverable in the 7-day trash (or empty trash with --purge)
vault restore <file> [--to <new_path>]    # recover a deleted note or attachment from the 7-day trash
vault reindex [--force] [--no-embed]      # refresh FTS5 + OCR + embeddings index (search/graph/ls also auto-reconcile)
vault setup --vault <path>                # configure local Obsidian vault path
vault setup --capture-note <path|daily> [--capture-section Captures] # configure default destination for `vault capture`
vault setup --encryption-key <passphrase> # end-to-end encrypt synced data (Argon2id-wrapped vault key); changing it re-wraps the key
vault setup --remote git:<url> [--branch main] # configure Git sync (or s3://<bucket>/<prefix> --endpoint <url>)
vault setup [--auto-sync true|false] [--auto-update true|false] [hooks] # toggle auto-sync/auto-update or install SessionStart hook + skill
vault register --email <e> [--password '<p>'] [--vault-id <id>] # create an account on Vault Cloud (prompts for password if omitted)
vault login --email <e> [--password '<p>']    # sign in to Vault Cloud (--mfa-code, --mfa-enroll, --mfa-verify, --token, --vault-id)
vault token [create|list|revoke <id>] [--name <label>] [--vault-id <id>] [--scope read|write] [--read-only] # manage scoped API keys (VAULT_API_KEY=vlt_...)
vault sync [--pull|--push]                # manual cloud sync with live progress (syncs notes, attachments & .obsidian/ config)
vault self-update [--check]               # check for new GitHub release and self-update binary in place
vault mcp                                 # start built-in stdio MCP server for Claude Desktop / Cursor
```

## Vault conventions

Output is TOON (compact). Errors are structured with a `code` and `help` suggestions.
Exit codes: 0 success/idempotent no-op, 1 error, 2 usage error.
