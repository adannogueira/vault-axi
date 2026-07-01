---
name: vault
description: Read, edit, and search an Obsidian markdown vault via the `vault` CLI. Use for any note/knowledge-base operation - reading a section or frontmatter, surgical mid-document edits, semantic + keyword search, following [[wikilinks]], and moving/renaming notes with link rewriting.
---

# vault - Obsidian vault as second brain

`vault` is an agent-ergonomic CLI over a single Obsidian vault folder. Files are the
source of truth; edits are surgical (no full-file rewrites); links use `[[wikilink]]`
syntax so the vault stays navigable in Obsidian. It works with Obsidian closed.

Run `vault` (no args) for a live dashboard, or `vault <command> --help` for any command.

## When to use

Reach for `vault` whenever you'd read from or write to the knowledge base: recalling
past decisions/research/debugging notes, saving a spec or brainstorm, updating a plan,
or exploring how notes connect.

## Reading (partial reads - don't slurp whole files)

```
vault read <file>                     # truncated body + char count
vault read <file> --outline           # heading structure only
vault read <file> --section "Design"  # one section (supports "Parent/Child" paths)
vault read <file> --frontmatter       # just the YAML frontmatter
vault read <file> --lines 40-80       # a line range
vault read <file> --full              # defeat truncation
```

## Editing (surgical - never rewrites the whole file)

```
vault write <file> --content "..."                                   # create (--force to overwrite; --content - reads stdin)
vault update <file> --section "Design" --mode replace --content "…"  # replace a section body IN PLACE
vault update <file> --section "Goals" --mode append --content "…"    # append within a section
vault update <file> --frontmatter status=active                      # set a frontmatter field (repeatable; omit value to unset)
vault update <file> --anchor "exact old text" --replace "new text"   # unique-match text swap
vault insert <file> --after-heading "Goals" --content "- new item"   # insert a block at a heading
vault insert <file> --append --content "…"                           # append / --prepend to the doc
```

`--section` targets by heading text; use a `Parent/Child` path to disambiguate. `--anchor`
errors unless the text appears exactly once, so edits are safe and idempotent.

## Searching & navigating

```
vault search "<query>"                 # hybrid: FTS5 keyword + semantic (RRF-fused)
vault search "<query>" --mode semantic # meaning only;  --mode fts for keyword only
vault related <file>                   # notes connected by [[links]] and by meaning
vault links <file>                     # outbound links (danglers flagged) + backlinks
vault ls [dir]                         # list notes with status/tags
```

## Organizing

```
vault move <old> <new>   # rename/move + rewrite every [[wikilink]] that pointed at it
vault delete <file>      # idempotent; warns which notes now have dangling links
vault reindex [--force]  # refresh the index (runs automatically before reads)
```

## Vault conventions

- Folders by type: `specs/`, `decisions/`, `research/`, `debugging/`, `brainstorms/`, `plans/`, `sprints/`, `drafts/`, `rules/`, `traps/`, `procedures/`.
- Filenames: lowercase kebab-case (`address-validation-research.md`).
- Frontmatter: `date`, `status` (draft|active|completed|superseded|archived), `tags`, `related` (wikilinks).
- Link generously with `[[note-name]]`. Mark superseded notes and link to their replacement.

Output is TOON (compact). Errors are structured with a `code` and `help` suggestions.
Exit codes: 0 success/idempotent no-op, 1 error, 2 usage error.
