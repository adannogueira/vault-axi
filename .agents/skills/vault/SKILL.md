---
name: vault
description: Read, edit, and search an Obsidian markdown vault via the `vault` CLI. Use for any note/knowledge-base operation - reading a section or frontmatter, surgical mid-document edits, task build logs, semantic + keyword search, following [[wikilinks]], and moving/renaming notes with link rewriting.
---

# vault - Obsidian vault as second brain

`vault` is an agent-ergonomic CLI over an Obsidian vault folder (with automatic cloud sync via Hosted Supabase/Turso, Git, or S3). Files are the source of truth; edits are surgical (no full-file rewrites); links use `[[wikilink]]` syntax so the vault stays navigable in Obsidian. It works with Obsidian closed.

Run `vault` (no args) for a live dashboard, or `vault <command> --help` for any command.

## When to use

Reach for `vault` whenever you'd read from or write to the knowledge base: recalling past decisions/research/debugging notes, saving a spec or brainstorm, logging build decisions/deviations during a task, updating a plan, or exploring how notes connect.

## Reading & Context Gathering (partial reads - don't slurp whole files)

```
vault read <file>                     # truncated body + char count
vault read <file> --outline           # heading structure only
vault read <file> --section "Design"  # one section (supports "Parent/Child" paths)
vault read <file> --frontmatter       # just the YAML frontmatter
vault read <file> --lines 40-80       # a line range
vault read <file> --full              # defeat truncation
vault context <file> [--limit 6]      # 1-call bundle: note + outlines/previews of connected [[wikilinks]]
```

## Creating & Editing Notes (100% self-contained in `vault`)

You do NOT need external file tools or MCPs to create or edit notes:

```
# Create a note with title, frontmatter, and body in one call:
vault write plans/my-plan.md --title "My Plan" --frontmatter status=draft --frontmatter tags="[axi, rust]" --from body.md
vault write drafts/quick.md --title "Quick Idea" --content "Initial body"

# Surgical section / frontmatter / anchor updates:
vault update <file> --section "Design" --mode replace --from body.md # replace a section body IN PLACE (or --content "…")
vault update <file> --section "Goals" --mode append --content "…"    # append within a section
vault update <file> --frontmatter status=active                      # set a frontmatter field (repeatable; omit value to unset)
vault update <file> --anchor "exact old text" --replace "new text"   # unique-match text swap

# Multi-operation atomic batch edits:
vault batch <file> --ops '[{"op":"section","heading":"Design","mode":"replace","content":"New design"},{"op":"frontmatter","key":"status","value":"active"}]'

# Insert blocks or toggle checklist items:
vault insert <file> --after-heading "Goals" --content "- new item"   # insert a block at a heading
vault insert <file> --append --from more.md                          # append / --prepend to the doc
vault check <file> "Acceptance criterion substring"                  # check (- [x]) or --uncheck (- [ ]) a task in place
vault check <file> --list                                            # list all checklist items in a note
```

## Task Build Logs (`vault log`)

While implementing a task that has a corresponding vault file, log entries with automatic timestamps into the task's `## Build Log` section (`--kind <label>` accepts any free-form string). Always pass the `<file>` explicitly so multiple concurrent agent sessions never collide:

```
vault log plans/my-task.md "Chose bundled SQLite FTS5 for zero-dependency builds" --kind decision
vault log plans/my-task.md "Found Windows file lock when running cargo test + build" --kind trap
vault log plans/my-task.md "Switched endpoint format" --kind deviation
```

## Searching, Filtering & Navigating

```
vault search "<query>"                                 # hybrid: FTS5 keyword + semantic (RRF-fused)
vault search "<query>" --status active --tag infra     # filter by frontmatter status/tag or --since 7d
vault search "<query>" --mode semantic                 # meaning only; --mode fts for keyword only
vault ls [dir] [--status active] [--tag axi] [--since 7d] # list notes with metadata filters
vault related <file>                                   # notes connected by [[links]] and by meaning
vault links <file>                                     # outbound links (danglers flagged) + backlinks
vault doctor                                           # audit dangling links, orphans, and missing frontmatter
```

## Organizing & Cloud Sync

```
vault move <old> <new>   # rename/move + rewrite every [[wikilink]] that pointed at it
vault delete <file>      # idempotent; warns which notes now have dangling links
vault reindex [--force]  # refresh the index (runs automatically before reads)
vault register --email <e> --password <p> # create an account on Vault Cloud (zero-config hosted sync)
vault login --email <e> --password <p>    # sign in to Vault Cloud (supports --mfa-code / --mfa-enroll)
vault sync [--pull|--push]                # manual cloud sync (runs automatically on reads/writes)
vault mcp                                 # start built-in stdio MCP server for Claude Desktop / Cursor
```

## Vault conventions

Output is TOON (compact). Errors are structured with a `code` and `help` suggestions.
Exit codes: 0 success/idempotent no-op, 1 error, 2 usage error.
