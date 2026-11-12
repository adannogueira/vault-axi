//! Helpers for tests that depend on process-global environment.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Points `VAULT_AXI_HOME` at `home` for the guard's lifetime and restores the previous value on
/// drop. Tests holding a guard run one at a time, so config and key lookups stay consistent.
pub struct HomeGuard {
    previous: Option<std::ffi::OsString>,
    _lock: MutexGuard<'static, ()>,
}

impl HomeGuard {
    pub fn new(home: &Path) -> Self {
        let lock = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = std::env::var_os("VAULT_AXI_HOME");
        std::env::set_var("VAULT_AXI_HOME", home);
        Self {
            previous,
            _lock: lock,
        }
    }
}

impl Drop for HomeGuard {
    fn drop(&mut self) {
        match &self.previous {
            Some(v) => std::env::set_var("VAULT_AXI_HOME", v),
            None => std::env::remove_var("VAULT_AXI_HOME"),
        }
    }
}
