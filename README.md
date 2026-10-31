# vault-axi

An [AXI](https://www.axi.md) (agent-ergonomic) CLI for reading, editing, and searching an
Obsidian markdown vault, compiled to a single standalone Rust binary (`vault`).

- **Surgical edits.** Replace/append/prepend a section *in place*, set a frontmatter field,
  or swap an exact text anchor.
- **Partial reads.** Fetch just the outline, one `## section`, the frontmatter, or a line range.
- **Real search.** SQLite **FTS5** keyword + **local semantic embeddings** (`TaylorAI/bge-micro-v2` via Hugging Face Candle) + a `[[wikilink]]` graph, fused with RRF.
- **Link-aware.** `related`/`links` traverse the wikilink graph; `move` rewrites backlinks vault-wide.
- **Works with Obsidian closed.** Files stay the source of truth; links stay Obsidian-compatible.

## Install

### From Source (Cargo)

```bash
cargo install --path .
vault setup --vault ~/Documents/obsidian-vault/Vault
vault reindex            # first run downloads the embedding model (~one-time)
vault setup hooks        # optional: load the dashboard at Claude Code session start
```

## Usage

Run `vault` for a dashboard, or `vault <command> --help`. See the command reference in
[`.agents/skills/vault/SKILL.md`](.agents/skills/vault/SKILL.md).

```bash
vault read plans/foo.md --section "Open Questions"
vault update plans/foo.md --section "Design" --mode replace --content "new body"
vault search "some text with similarity"
vault related decisions/some-decision.md
vault move drafts/idea.md plans/idea.md
```

## How it works

- The index lives at `~/.vault-axi/index.db` (SQLite with bundled FTS5, outside the vault).
- Every read command runs a cheap `reconcile` (mtime/size diff) first, so the index tracks
  files edited in Obsidian or elsewhere. Embeddings backfill lazily and on `vault reindex`.
- Markdown edits splice raw line ranges derived from a heading scan (fenced code and
  frontmatter aware), the document is never round-tripped through a parser.
- `vault setup hooks` installs the Claude Code `SessionStart` hook and writes the embedded
  `SKILL.md` directly into `~/.claude/skills/vault/SKILL.md`.

## Config

- `~/.vault-axi/config.json` — `{ "vaultPath": "…" }`.
- `VAULT_AXI_VAULT` — override the vault path.
- `VAULT_AXI_NO_EMBED=1` — disable embeddings (FTS5 + graph only).
- `VAULT_AXI_EMBED_MODEL` — override the embedding model id.

## Development

```bash
cargo test
cargo build --release
```
