use crate::axi::AxiError;
use crate::config::resolve_vault_path;
use crate::db::open_db;
use crate::index::reconcile::{reconcile, ReconcileOpts};
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

    pub fn require_vault(&self) -> Result<String, AxiError> {
        self.vault_path.clone().ok_or_else(|| {
            AxiError::new(
                "No vault configured",
                "VALIDATION_ERROR",
                vec![
                    "Run `vault setup --vault <path>` to point at your Obsidian vault".to_string(),
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

    pub fn sync(&mut self, opts: ReconcileOpts) -> Result<(), AxiError> {
        let root = self.require_vault()?;
        let db = self.db()?;
        reconcile(db, &root, opts);
        Ok(())
    }
}
