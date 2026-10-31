use crate::axi::AxiError;
use crate::config::resolve_vault_path;
use crate::db::open_db;
use crate::index::reconcile::{reconcile, ReconcileOpts};
use crate::sync::{auto_pull_if_needed, auto_push_if_needed};
use rusqlite::Connection;

pub struct Ctx {
    pub vault_path: Option<String>,
    db_handle: Option<Connection>,
}

impl Ctx {
    pub fn new() -> Self {
        Self {
            vault_path: resolve_vault_path(),
            db_handle: None,
        }
    }

    pub fn with_vault(vault_path: impl Into<String>) -> Self {
        Self {
            vault_path: Some(crate::config::resolve_abs_string(&vault_path.into())),
            db_handle: None,
        }
    }

    pub fn with_vault_and_db(vault_path: impl Into<String>, db: Connection) -> Self {
        Self {
            vault_path: Some(crate::config::resolve_abs_string(&vault_path.into())),
            db_handle: Some(db),
        }
    }

    pub fn require_vault(&self) -> Result<String, AxiError> {
        self.vault_path.clone().ok_or_else(|| {
            AxiError::new(
                "No vault configured",
                "VALIDATION_ERROR",
                vec![
                    "Run `vault setup --vault <path>` to point at your Obsidian vault".to_string(),
                    "Or run `vault login --email <email> --password <password>` for a hosted cloud vault".to_string(),
                    "Or set VAULT_AXI_VAULT=<path>".to_string(),
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

    /// Bring the index up to date (pulling from cloud if configured and due).
    pub fn sync(&mut self, opts: ReconcileOpts) -> Result<(), AxiError> {
        let root = self.require_vault()?;
        let db = self.db()?;
        let _ = auto_pull_if_needed(db, &root, opts.force);
        reconcile(db, &root, opts);
        Ok(())
    }

    /// Reconcile local changes and push to cloud if configured.
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
        let _ = auto_push_if_needed(db, &root);
        Ok(())
    }
}
