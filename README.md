# vault-axi

An [AXI](https://www.axi.md) (agent-ergonomic) CLI and built-in MCP server for reading, editing,
and searching an Obsidian markdown vault, compiled to a single standalone Rust binary (`vault`).

- **Zero-config Hosted Cloud + Self-Hosted Sync.** Register or sign in with `vault register` / `vault login` (with optional TOTP MFA) on our hosted multi-tenant cloud for instant automatic sync, or point `--remote` at a **Git** repository or **S3 / Cloudflare R2** bucket.
- **First-Class Task Build Logs (`vault log`).** Append timestamped entries with any custom `--kind <label>` directly into a task note's `## Build Log` section (`vault log plans/my-task.md "..." [--kind <label>] [--section <heading>]`)—safe across concurrent multi-tab sessions with zero global state collisions.
- **Partial Reads & 1-Call Context.** Fetch just the outline, one `## section`, frontmatter, a line range, or a full `vault context` bundle (target note + connected `[[wikilink]]` outlines and previews).
- **Fast Multilingual Hybrid Search & Graph Discovery.** Combines SQLite **FTS5** (`unicode61` diacritic-folding + weighted BM25) + **zero-dependency multilingual stem/cognate/subword vectors** + 1-hop & 2-hop `[[wikilink]]` graph proximity fused with RRF. Works 100% offline.
- **Built-in MCP Server (`vault mcp`).** Run `vault mcp` to expose the entire toolset over stdio JSON-RPC for Claude Desktop, Cursor, or any MCP client.

## Install

### One-Line Install (macOS, Linux, WSL, Git Bash)

```bash
curl -fsSL https://raw.githubusercontent.com/adannogueira/vault-axi/main/install.sh | bash
```

### Windows (PowerShell)

```powershell
irm https://raw.githubusercontent.com/adannogueira/vault-axi/main/install.ps1 | iex
```

*(Downloads the latest prebuilt binary for your OS/CPU, verifies its SHA-256 checksum, installs `vault` to `~/.local/bin`, and automatically adds it to your `PATH`.)*

### From Source (Cargo)

```bash
cargo install --path .
```

## Quick Start

### Option 1: Hosted Cloud Vault (Zero Manual Setup)
For users who want instant cloud sync across devices, headless agent containers, or Claude Desktop without setting up Git, AWS S3, or their own database:
```bash
vault register --email you@example.com
# Prompts for password interactively (safe for '!', '$', etc.), or pass --password 'your!pass' in single quotes
# To sign in on another machine (supports TOTP MFA via --mfa-code, --mfa-enroll, --mfa-verify):
vault login --email you@example.com [--mfa-code <code>]
vault setup hooks        # optional: install Claude Code SessionStart hook + skill

# For headless containers / CI / remote agent sandboxes (zero interactive login):
vault token create --name "agent-container"   # prints vlt_... API key
# Inside the container, simply set:
#   export VAULT_API_KEY="vlt_..."
# `vault` will silently authenticate and bootstrap ~/.vault-axi/cloud-vault on the first command!
```

### Option 2: Local Obsidian Vault (+ Optional Git or S3 Sync)
```bash
vault setup --vault ~/Documents/obsidian-vault/Vault
vault setup --remote git:git@github.com:you/vault.git   # optional automatic Git sync
vault reindex            # builds FTS5 + multilingual vector & wikilink index instantly
vault setup hooks        # optional: load the dashboard at Claude Code session start
```

## Usage

Run `vault` for a dashboard, or `vault <command> --help`. See the full agent skill reference in
[`.agents/skills/vault/SKILL.md`](.agents/skills/vault/SKILL.md).

```bash
# Reading, 1-call context bundle & path resolution
vault read plans/foo.md --section "Open Questions"
vault read plans/foo.md --outline
vault context plans/foo.md --limit 6
vault path plans/foo.md

# Creating & surgical editing
vault write plans/foo.md --title "Foo Plan" --frontmatter status=active --from body.md
vault update plans/foo.md --section "Design" --mode replace --content "New body"
vault insert plans/foo.md --after-heading "Design" --content "- Additional item"
vault batch plans/foo.md --ops '[{"op":"section","heading":"Design","mode":"append","content":"More"},{"op":"frontmatter","key":"status","value":"done"}]'

# Task Build Logs & Checklists (explicitly scoped per task file so multi-tab sessions never collide)
vault log plans/foo.md "Switched to bundled SQLite FTS5" --kind decision
vault check plans/foo.md "Ship single binary"
vault check plans/foo.md --list

# Search, Graph, Organizing & Health
vault search "targeted alerts" --status active --tag infra --since 7d
vault ls plans --status active --since 7d --limit 20
vault related decisions/some-decision.md --limit 10
vault links plans/foo.md
vault move plans/foo.md archive/foo.md
vault delete archive/foo.md
vault doctor
vault sync
```

## Self-Hosted Cloud Options (Git & S3 / Cloudflare R2)

For self-hosted vaults, `vault` supports two backends out of the box:

```bash
# Self-hosted Git repository (automatic pull --rebase / commit / push)
vault setup --remote git:git@github.com:you/vault.git --branch main

# Self-hosted S3 or Cloudflare R2 bucket
vault setup --remote s3://my-vault-bucket/notes --endpoint https://<account>.r2.cloudflarestorage.com
```

## Development

```bash
cargo test
cargo build --release
```
