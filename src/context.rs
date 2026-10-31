use crate::axi::AxiError;
use crate::config::resolve_vault_path;
use crate::db::open_db;
use crate::index::reconcile::{reconcile, ReconcileOpts};
use crate::sync::{auto_pull_if_needed, auto_push_if_needed, ConflictInfo};
use rusqlite::Connection;
use serde_json::{json, Value};

pub struct Ctx {
    pub vault_path: Option<String>,
    db_handle: Option<Connection>,
    pub sync_conflicts: Vec<ConflictInfo>,
}

impl Ctx {
    pub fn new() -> Self {
        Self {
            vault_path: resolve_vault_path(),
            db_handle: None,
            sync_conflicts: Vec::new(),
        }
    }

    pub fn with_vault(vault_path: impl Into<String>) -> Self {
        Self {
            vault_path: Some(crate::config::resolve_abs_string(&vault_path.into())),
            db_handle: None,
            sync_conflicts: Vec::new(),
        }
    }

    pub fn with_vault_and_db(vault_path: impl Into<String>, db: Connection) -> Self {
        Self {
            vault_path: Some(crate::config::resolve_abs_string(&vault_path.into())),
            db_handle: Some(db),
            sync_conflicts: Vec::new(),
        }
    }

    pub fn require_vault(&self) -> Result<String, AxiError> {
        self.vault_path.clone().ok_or_else(|| {
            AxiError::new(
                "No vault configured",
                "VALIDATION_ERROR",
                vec![
                    "Run `vault setup --vault <path>` to point at your Obsidian vault".to_string(),
                    "Or run `vault login --email <email>` for a hosted cloud vault".to_string(),
                    "Or set VAULT_API_KEY=vlt_... (for headless containers) or VAULT_AXI_VAULT=<path>".to_string(),
                ],
            )
        })
    }

    pub fn db(&mut self) -> Result<&mut Connection, AxiError> {
        if self.db_handle.is_none() {
            let conn = open_db().map_err(|e| {
                AxiError::new(
                    format!("Failed to open index database: {e}"),
                    "INTERNAL_ERROR",
                    vec![],
                )
            })?;
            self.db_handle = Some(conn);
        }
        Ok(self.db_handle.as_mut().unwrap())
    }

    /// Force a cloud pull before mutating a local file so edits made seconds ago on another
    /// device are pulled before local modification begins.
    pub fn pull_before_mutation(&mut self) -> Result<(), AxiError> {
        if std::env::var("VAULT_AXI_NO_SYNC").as_deref() == Ok("1")
            || crate::config::load_config().remote.is_none()
        {
            return Ok(());
        }
        let root = self.require_vault()?;
        let db = self.db()?;
        let summary = auto_pull_if_needed(db, &root, true);
        if let Some(s) = summary {
            let changed = s.pulled > 0 || s.deleted_local > 0 || !s.conflicts.is_empty();
            if changed {
                reconcile(
                    db,
                    &root,
                    ReconcileOpts {
                        force: false,
                        with_embeddings: Some(false),
                    },
                );
            }
            self.sync_conflicts.extend(s.conflicts);
        }
        Ok(())
    }

    /// Pull from cloud if the cooldown has elapsed (used before `read`).
    pub fn pull_if_due(&mut self) -> Result<(), AxiError> {
        if std::env::var("VAULT_AXI_NO_SYNC").as_deref() == Ok("1")
            || crate::config::load_config().remote.is_none()
        {
            return Ok(());
        }
        let root = self.require_vault()?;
        let db = self.db()?;
        let summary = auto_pull_if_needed(db, &root, false);
        if let Some(s) = summary {
            let changed = s.pulled > 0 || s.deleted_local > 0 || !s.conflicts.is_empty();
            if changed {
                reconcile(
                    db,
                    &root,
                    ReconcileOpts {
                        force: false,
                        with_embeddings: Some(false),
                    },
                );
            }
            self.sync_conflicts.extend(s.conflicts);
        }
        Ok(())
    }

    /// Bring the index up to date (pulling from cloud if configured and due).
    pub fn sync(&mut self, opts: ReconcileOpts) -> Result<(), AxiError> {
        let root = self.require_vault()?;
        let db = self.db()?;
        let summary = auto_pull_if_needed(db, &root, opts.force);
        reconcile(db, &root, opts);
        if let Some(s) = summary {
            self.sync_conflicts.extend(s.conflicts);
        }
        Ok(())
    }

    /// Reconcile local changes and push to cloud if configured (with 3-way merge / conflict detection).
    pub fn sync_after_mutation(&mut self) -> Result<(), AxiError> {
        let root = self.require_vault()?;
        let db = self.db()?;
        reconcile(
            db,
            &root,
            ReconcileOpts {
                force: false,
                with_embeddings: Some(false),
            },
        );
        if let Some(s) = auto_push_if_needed(db, &root) {
            let changed = s.pulled > 0 || s.deleted_local > 0 || !s.conflicts.is_empty();
            if changed {
                reconcile(
                    db,
                    &root,
                    ReconcileOpts {
                        force: false,
                        with_embeddings: Some(false),
                    },
                );
            }
            self.sync_conflicts.extend(s.conflicts);
        }
        Ok(())
    }

    /// If any sync conflict or auto-merge occurred during this command, attach `warning`
    /// and `conflicts` to the returned JSON object so the last writer is notified.
    pub fn attach_sync_warnings(&mut self, val: &mut Value) {
        if self.sync_conflicts.is_empty() {
            return;
        }
        if let Value::Object(map) = val {
            let conflicts = std::mem::take(&mut self.sync_conflicts);
            let joined_msg = conflicts
                .iter()
                .map(|c| c.message.as_str())
                .collect::<Vec<_>>()
                .join(" | ");
            map.insert("warning".to_string(), json!(joined_msg));
            map.insert(
                "conflicts".to_string(),
                serde_json::to_value(&conflicts).unwrap_or_else(|_| json!([])),
            );
        }
    }
}
