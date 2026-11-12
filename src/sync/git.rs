use super::SyncSummary;
use crate::axi::AxiError;
use std::path::Path;
use std::process::Command;

pub(super) fn sync_git(
    vault_root: &str,
    repo_url: Option<&str>,
    branch: Option<&str>,
    push: bool,
) -> Result<SyncSummary, AxiError> {
    let git_dir = Path::new(vault_root).join(".git");
    if !git_dir.exists() {
        if let Some(url) = repo_url {
            let _ = Command::new("git")
                .args(["init"])
                .current_dir(vault_root)
                .output();
            let _ = Command::new("git")
                .args(["remote", "add", "origin", url])
                .current_dir(vault_root)
                .output();
        } else {
            return Err(AxiError::new(
                "Vault is not a git repository and no remote URL is configured",
                "SYNC_ERROR",
                vec!["Run `vault setup --remote git:<repo-url>`".to_string()],
            ));
        }
    }

    let br = branch.unwrap_or("main");
    let mut pulled = 0;
    let mut pushed = 0;

    let pull_out = Command::new("git")
        .args(["pull", "--rebase", "--autostash", "origin", br])
        .current_dir(vault_root)
        .output();
    if let Ok(out) = pull_out {
        let stdout = String::from_utf8_lossy(&out.stdout);
        if stdout.contains("Updating") || stdout.contains("Fast-forward") {
            pulled = 1;
        }
    }

    if push {
        let status_out = Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(vault_root)
            .output()
            .map_err(|e| AxiError::new(format!("git error: {e}"), "SYNC_ERROR", vec![]))?;
        let dirty = !String::from_utf8_lossy(&status_out.stdout)
            .trim()
            .is_empty();
        if dirty {
            let _ = Command::new("git")
                .args(["add", "-A"])
                .current_dir(vault_root)
                .output();
            let _ = Command::new("git")
                .args(["commit", "-m", "vault-axi: auto-sync"])
                .current_dir(vault_root)
                .output();
            let push_res = Command::new("git")
                .args(["push", "-u", "origin", br])
                .current_dir(vault_root)
                .output();
            if let Ok(p) = push_res {
                if p.status.success() {
                    pushed = 1;
                }
            }
        }
    }

    Ok(SyncSummary {
        backend: "git".to_string(),
        pulled,
        pushed,
        message: Some(format!("Git sync complete (branch: {br})")),
        ..Default::default()
    })
}
