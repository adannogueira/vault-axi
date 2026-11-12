---
date: 2026-11-12
status: draft
---

# Mobile sync plugin + crypto v2

## Problem

Obsidian on Android/iOS can't run the `vault` CLI, so a phone vault never syncs. The S3 and hosted-cloud backends store data in vault-axi's own encrypted envelope (`enc:v1:`), so no off-the-shelf mobile app can read them.

The current crypto is also weak:

- With no `--encryption-key`, the cloud key is `PBKDF2("vault-axi-cloud-e2ee:{user_id}:{vault_id}")` ([crypto.rs:55](../../src/crypto.rs)). The server knows both inputs, so it can decrypt everything.
- With a passphrase, the AES key is `PBKDF2(passphrase)` with a fixed global salt and 10k iterations. There is no key wrapping, so changing the passphrase means re-encrypting all data.
- Ciphertexts aren't bound to their path, so a server could swap note bodies without detection.

## Goals

1. An Obsidian plugin (desktop + mobile) that syncs with the **hosted cloud** and **S3/R2** backends, interoperating with the CLI on the same vault.
2. **Crypto v2** for users who set `--encryption-key`: a random per-vault data key, wrapped by an Argon2id passphrase key, stored in a keyring that syncs with the data.
3. **Nothing written before this change becomes unreadable.** Every `enc:v1:` payload (remote rows, S3 objects, local `sync_state`/`note_versions`/`trash`) keeps decrypting after the upgrade, in both the CLI and the plugin.
4. **One implementation of sync logic.** Merge, crypto, path rules and the sync planner live in one Rust core used natively by the CLI and as WASM by the plugin.

## Non-goals

- Git backend in the plugin (Git data is plaintext; mobile users can use the Obsidian Git plugin).
- Making encryption mandatory. No key set → v1 derived key, exactly as today.
- Key rotation after compromise (new data key + re-encrypt everything). The v2 format allows it later.
- SigV4-signed S3 requests. Today's S3 backend uses unsigned requests to a bucket the user exposes; the plugin matches that. Tracked as follow-up.
- Supabase schema changes (the server isn't in this repo). The keyring rides in an existing table row.

## Decisions

| Decision | Choice |
|---|---|
| Repo layout | Monorepo: root package stays the CLI; add `crates/core`, `crates/core-wasm`, `plugin/` |
| Encryption when no key is set | Optional; v1 derived key unchanged |
| KDF | Argon2id (m=64 MiB, t=3, p=1), params stored in the keyring |
| Plugin backends v1 | Hosted cloud + S3 |
| Crypto library | RustCrypto (`aes-gcm`, `argon2`, `pbkdf2`, `sha1`, `sha2`): pure Rust, builds for `wasm32-unknown-unknown`. `ring` is dropped from the core |

## Design

### Workspace layout

```
Cargo.toml              # [workspace] + the existing `vault` binary package
src/                    # CLI (I/O: fs, sqlite, ureq, git)
crates/core/            # vault-axi-core: no I/O, no std::fs, no network
  src/crypto/{mod,v1,v2,keyring}.rs
  src/payload.rs        # b64:v1 binary encoding
  src/merge.rs          # merge_three_way (moved from sync.rs)
  src/paths.rs          # syncable-path rules, conflict-copy naming, reserved paths
  src/plan.rs           # sync planner
  tests/fixtures/       # frozen v1 ciphertexts, cross-language vectors
crates/core-wasm/       # wasm-bindgen facade over core
plugin/                 # Obsidian plugin (TypeScript, esbuild, wasm inlined)
```

### Crypto v2

**Keyring** (one per vault, plaintext JSON, contains no secrets in the clear):

```json
{
  "version": 2,
  "keyId": "k2_<16 hex>",
  "kdf": { "alg": "argon2id", "m": 65536, "t": 3, "p": 1, "salt": "<b64 16 bytes>" },
  "wrappedKey": "<b64 nonce || AES-256-GCM(KEK, DEK)>",
  "createdAt": "<rfc3339>"
}
```

- `DEK` is 32 random bytes. `KEK = Argon2id(passphrase, salt, params)`. Wrapping AAD = `"vault-axi/keyring/v2/" + keyId`.
- `keyId` is random, not derived from key material.
- Changing the passphrase rewraps the DEK (new salt). No data is re-encrypted.

**Payload envelope**: `enc:v2:<keyId>:<nonce_b64>:<ct_b64>`, AES-256-GCM, AAD = `"vault-axi/v2/" + keyId + "/" + path`. Binding the path means a swapped or renamed ciphertext fails authentication. Local DB rows use the same path as their sync path. Moving a note re-encrypts it on push (the planner already treats a move as delete + create).

**Storage of the keyring**

- Hosted cloud: a `vault_notes` row at the reserved path `.vault-axi/keyring.json` (content is the JSON, `enc_key_id = "keyring"`). Created with a plain `POST` (no `merge-duplicates`), so two devices racing to create one get a 409 and the loser re-reads.
- S3: object `<prefix>/.vault-axi/keyring.json`, created with `If-None-Match: *`.
- `.vault-axi/` is a reserved prefix: never listed as a local syncable file, never pulled to disk.

**Decryption order (`Keychain`)**

`Keychain` holds the optional v2 DEK plus an ordered list of legacy v1 keys. `decrypt(payload, path)`:

1. No prefix → plaintext passthrough (unchanged legacy behavior).
2. `enc:v2:` → DEK whose `keyId` matches; fail loudly otherwise (never fall through to v1).
3. `enc:v1:` → try, in order: configured passphrase via legacy PBKDF2; previous passphrase (`localEncryptionKey`, raw-32-byte b64 and derived forms); cloud-derived key; local-derived key. This is the current `decrypt_at_rest` chain ([crypto.rs:290](../../src/crypto.rs)), moved verbatim.

`encrypt(plaintext, path)` → v2 if a DEK is unlocked, otherwise v1 with the active legacy key (today's behavior for users without a passphrase).

**Unlock and caching (CLI)**

- Argon2id at 64 MiB costs ~0.3-1s, too slow to pay on every command (auto-pull runs before reads). After unlocking, the CLI caches the DEK in `~/.config/vault-axi/keys/<vault-hash>.json` (0600) as `{keyId, dek_b64, passphraseHash}`. The cache entry is ignored when `sha256(passphrase)` changes.
- This doesn't widen exposure: the passphrase itself already sits in `config.json`.

**Migration (existing user with `--encryption-key`, upgraded CLI)**

On the first sync after upgrading:

1. Fetch the keyring. If it's absent, create a DEK and a keyring, upload with create-if-absent, and re-fetch on conflict.
2. If it's present, unwrap with the passphrase. Wrong passphrase → hard error `ENCRYPTION_KEY_MISMATCH`; nothing is written, nothing is pushed.
3. `current_key_id` becomes the keyring's `keyId`. Every remote row whose `enc_key_id` differs is re-uploaded by the existing mismatch path ([sync.rs:1416](../../src/sync.rs)). Pull always happens first, so newer remote content is decrypted via v1 before being re-encrypted as v2.
4. Local DB rows (`sync_state`, `note_versions`, `trash`) are re-encrypted to v2 by `reencrypt_local_db`, which now takes a `Keychain`.
5. Rows the CLI couldn't migrate (offline, read-only token) remain v1 and still decrypt. Migration is idempotent and resumes on the next sync.

Users without `--encryption-key` see no change: no keyring, `enc:v1:` with the derived key.

**Turning encryption on/off/changing it**

- `vault setup --encryption-key <new>` with an existing keyring → rewrap the DEK under the new passphrase and upload the keyring (conditional on the old keyring's hash/ETag).
- Setting it for the first time → the migration above.
- Clearing it while a keyring exists → refuse with a hint (would orphan v2 data). Explicit `--disable-encryption` decrypts back to v1 is out of scope.

### Sync planner (shared)

`sync_hosted_cloud` today interleaves the decisions (download / upload / merge / delete / conflict) with HTTP and fs calls in ~830 lines. The decisions move into `core::plan` as pure functions:

```rust
pub struct LocalEntry  { path, hash }
pub struct BaseEntry   { path, remote_hash }            // sync_state
pub struct RemoteEntry { path, hash, deleted, key_id }
pub enum Action {
    Download { path },                                  // fast-forward pull
    Merge { path },                                     // both changed: fetch remote, then resolve()
    Upload { path, expected_remote_hash: Option<_> },   // conditional PATCH or insert
    Reencrypt { path },                                 // key_id mismatch, content unchanged
    DeleteRemote { path },
    DeleteLocal { path },                               // to trash
    KeepLocalOverRemoteDelete { path },
    RestoreRemoteOverLocalDelete { path },
    AdoptBase { path, hash },                           // local == remote, refresh sync_state
}
pub fn plan(local, base, remote, current_key_id, mode) -> Vec<Action>;
pub fn resolve(path, base, local, remote) -> Resolution; // Clean | AutoMerged | ConflictCopy{copy_path}
```

The CLI and the plugin execute actions with their own I/O (sqlite + fs + ureq vs IndexedDB + `app.vault.adapter` + `requestUrl`). Every rule currently documented in `sync_hosted_cloud` (delete-vs-edit, restore-over-delete, conditional PATCH with refetch on 0 rows, batching of new/re-encrypted rows ≤48 KB) is ported as a planner rule with a table-driven test.

**S3 moves onto the same planner.** Today S3 has no 3-way merge and no deletion propagation. Manifest v2:

```json
{ "version": 2, "entries": { "<path>": { "hash": "", "keyId": "", "deleted": false, "mtime": 0 } } }
```

v1 manifests (`{"path": "hash:keyid"}`) are still read. Manifest writes use `If-Match: <etag>`. On 412 → re-read the manifest and re-plan.

### Syncable path rules (shared)

`paths.rs` owns what counts as syncable (markdown, attachments ≤25 MB, `.obsidian/**` ≤10 MB minus `workspace*.json`, `cache`, `.DS_Store`). New exclusions:

- `.vault-axi/**` (reserved)
- `.obsidian/plugins/vault-axi-sync/data.json`. The plugin keeps no secrets there, but its settings are per-device.

`vault.rs` keeps the filesystem walk but delegates the accept/reject decision to `paths::is_syncable`, so CLI and plugin agree on the file set.

### Plugin

- **Id** `vault-axi-sync`, `isDesktopOnly: false`, min Obsidian version: the first with `requestUrl` + `app.saveLocalStorage` (1.1.x; pin during scaffold).
- **Settings tab**: backend (Hosted cloud | S3), cloud sign-in (email + password + TOTP if MFA is on, or paste a `vlt_` token), S3 endpoint/bucket/prefix, encryption passphrase (optional), auto-sync toggle, interval.
- **Secrets** (tokens, passphrase, unwrapped DEK) go in `app.saveLocalStorage` (per-device, outside the vault). Never in `data.json`, which lives in the vault.
- **Sync state** (`sync_state` equivalent, base contents encrypted with the active key) in IndexedDB, keyed by vault id.
- **Transport**: `requestUrl` (no CORS issues on mobile). Same PostgREST calls and headers as the CLI. Token refresh via `/auth/v1/token?grant_type=refresh_token`; `vlt_` exchange via `/functions/v1/token-exchange`.
- **File enumeration**: `app.vault.adapter.list` recursive (needed for `.obsidian/**`, which `vault.getFiles()` omits), filtered by `core.isSyncable`. Binary files are read with `readBinary` and encoded to `b64:v1:`.
- **Triggers**: on layout ready; on `modify`/`create`/`delete`/`rename`, debounced 3 s; on app resume (`visibilitychange`); every N minutes (default 5). A single-flight lock prevents overlapping runs.
- **UX**: status bar item (idle / syncing n/m / error), ribbon icon + "Sync now" command, a Notice per conflict with a link to the conflict copy, "Unlock" modal if a keyring exists and no passphrase is stored.
- **Unlock cost on phones**: Argon2id (64 MiB) runs once per device in WASM, then the DEK is cached in local storage.
- **WASM**: `core-wasm` built with `wasm-pack --target web`, inlined as base64 into `main.js` by esbuild (Obsidian plugins ship as a single JS file).

### What the plugin does not do

Search index, embeddings, OCR, history/versions UI. Version history recorded during sync (`record_edit_transition`) stays CLI-only; the plugin keeps a simple local base for merges.

## Verification

- **Frozen v1 fixtures (written before any code moves):** ciphertexts produced by today's code for each legacy key source (passphrase, previous passphrase raw/derived, cloud-derived, local-derived, binary payload). Asserted in core tests forever, and by the plugin's WASM tests.
- **Cross-language vectors:** v2 envelope + keyring vectors generated by Rust, decrypted by the plugin tests.
- **Planner tables:** one test per rule (the current sync.rs has 2 tests; this is the main coverage gain).
- **CLI parity:** existing `cargo test` stays green after each phase; `cargo clippy -- -D warnings` added to CI.
- **Migration test:** temp vault + fake PostgREST (small in-process HTTP server in `tests/`) with v1 rows → upgraded sync → all rows v2, all files byte-identical, wrong passphrase writes nothing.
- **Plugin:** `vitest` + `tsc --noEmit` + `eslint`. Engine tests run the real WASM against an in-memory transport and adapter.
- **End to end:** CLI vault and Obsidian desktop with `app.emulateMobile(true)` on the same cloud vault: create/edit/delete/conflict both ways, v1→v2 migration mid-flight. Android device/emulator if available.

## Open questions

- Hosted Supabase RLS / triggers: confirm a row at `.vault-axi/keyring.json` is allowed and that a non-upsert `POST` returns 409 on the `(user_id, vault_id, path)` unique key. Verified in phase 3 against the live project.
- Does the hosted `vault_notes.content` column have a size limit that v2 overhead (keyId in prefix) could cross? Overhead is ~25 bytes, so unlikely.

## Phases

Each phase touches ≤5 files where possible, ends green (`cargo fmt --check`, `cargo clippy -D warnings`, `cargo test`, plus plugin checks once it exists), and is marked here when done.

| # | Phase | Status |
|---|---|---|
| 0 | Freeze v1 fixtures from current code + tests | pending |
| 1 | Cargo workspace + `core` crate: move v1 crypto (RustCrypto), payload, merge, conflict naming; CLI depends on core | pending |
| 2 | Core crypto v2: keyring, Argon2id, v2 envelope with path AAD, `Keychain` | pending |
| 3 | CLI on `Keychain`: keyring fetch/create (cloud + S3), DEK cache, migration, setup rewrap, `doctor` encryption status | pending |
| 4 | Core `paths` + `plan`; refactor `sync_hosted_cloud` onto the planner (parity tests) | pending |
| 5 | S3 onto the planner, manifest v2, conditional writes | pending |
| 6 | `core-wasm` bindings + build; node test harness against fixtures | pending |
| 7 | Plugin scaffold: manifest, settings, secrets storage, auth (cloud + S3), unlock modal | pending |
| 8 | Plugin sync engine: transports, IndexedDB state, executor, triggers, status/notices | pending |
| 9 | CI (wasm build, plugin lint/test, clippy) + release assets (`main.js`, `manifest.json`, `styles.css`) + README | pending |
| 10 | End-to-end verification CLI ↔ plugin, migration mid-flight | pending |
