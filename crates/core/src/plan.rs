//! The sync planner: given what is on disk, what was last synced (the base) and what the remote
//! holds, decide what to do with every path. Pure and deterministic; clients execute the actions
//! with their own storage and transport.
//!
//! A sync runs in two passes so the push sees the result of the pull:
//! 1. [`plan_pull`], executed;
//! 2. local files re-scanned, then [`plan_push`], executed.

use crate::merge::{merge_three_way, MergeOutcome};
use crate::paths::{conflict_copy_path, is_reserved};
use crate::payload;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalFile {
    pub path: String,
    /// [`crate::hash::sha1_hex`] of the file's sync payload.
    pub hash: String,
}

/// What this client last agreed with the remote on, per path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BaseEntry {
    pub path: String,
    pub remote_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteFile {
    pub path: String,
    pub hash: String,
    #[serde(default)]
    pub deleted: bool,
    /// Key the remote payload is encrypted under; empty when the backend doesn't record it.
    #[serde(default)]
    pub key_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "camelCase")]
pub enum Action {
    /// Local already equals the remote; record it as the new base without transferring anything.
    AdoptBase { path: String, hash: String },
    /// Remote changed and local did not: overwrite local with the remote version.
    Download { path: String },
    /// Both sides changed since the base: fetch the remote and [`resolve`] it.
    Merge { path: String },
    /// Deleted remotely and untouched locally: remove the local file.
    DeleteLocal { path: String },
    /// Deleted remotely but edited locally: keep the edit and forget the base so it re-uploads.
    KeepLocalOverRemoteDelete { path: String },
    /// Deleted on both sides: just forget the base.
    ForgetBase { path: String },
    /// Send the local file. `expected_remote_hash` is the live remote version the upload replaces
    /// (for compare-and-swap); `None` when the remote has no live version.
    #[serde(rename_all = "camelCase")]
    Upload {
        path: String,
        expected_remote_hash: Option<String>,
    },
    /// Deleted locally and unchanged remotely: tombstone it remotely.
    DeleteRemote { path: String },
    /// Deleted locally but edited remotely: bring the remote edit back.
    RestoreRemote { path: String },
}

struct Index<'a> {
    local: HashMap<&'a str, &'a LocalFile>,
    base: HashMap<&'a str, &'a BaseEntry>,
    remote: BTreeMap<&'a str, &'a RemoteFile>,
}

impl<'a> Index<'a> {
    fn new(local: &'a [LocalFile], base: &'a [BaseEntry], remote: &'a [RemoteFile]) -> Self {
        Self {
            local: local
                .iter()
                .filter(|f| !is_reserved(&f.path))
                .map(|f| (f.path.as_str(), f))
                .collect(),
            base: base.iter().map(|b| (b.path.as_str(), b)).collect(),
            remote: remote
                .iter()
                .filter(|f| !is_reserved(&f.path))
                .map(|f| (f.path.as_str(), f))
                .collect(),
        }
    }

    fn base_hash(&self, path: &str) -> Option<&'a str> {
        self.base.get(path).map(|b| b.remote_hash.as_str())
    }

    /// Local differs from what was last synced (or was never synced).
    fn locally_modified(&self, path: &str) -> bool {
        match self.local.get(path) {
            None => false,
            Some(l) => self.base_hash(path) != Some(l.hash.as_str()),
        }
    }

    fn remotely_modified(&self, remote: &RemoteFile) -> bool {
        self.base_hash(&remote.path) != Some(remote.hash.as_str())
    }
}

/// Plan the pull pass. With `pull_all = false` (a push-only sync) only paths that changed on both
/// sides are planned, so a push never overwrites a concurrent remote edit.
pub fn plan_pull(
    local: &[LocalFile],
    base: &[BaseEntry],
    remote: &[RemoteFile],
    pull_all: bool,
) -> Vec<Action> {
    let ix = Index::new(local, base, remote);
    let mut actions = Vec::new();
    for (&path, r) in &ix.remote {
        let path = path.to_string();
        let local = ix.local.get(r.path.as_str());

        if r.deleted {
            if !pull_all {
                continue;
            }
            match local {
                Some(_) if ix.locally_modified(&path) => {
                    actions.push(Action::KeepLocalOverRemoteDelete { path })
                }
                Some(_) => actions.push(Action::DeleteLocal { path }),
                None if ix.base.contains_key(path.as_str()) => {
                    actions.push(Action::ForgetBase { path })
                }
                None => {}
            }
            continue;
        }

        if local.is_some_and(|l| l.hash == r.hash) {
            if ix.base_hash(&path) != Some(r.hash.as_str()) {
                actions.push(Action::AdoptBase {
                    path,
                    hash: r.hash.clone(),
                });
            }
            continue;
        }
        if !ix.remotely_modified(r) {
            continue;
        }
        if ix.locally_modified(&path) {
            actions.push(Action::Merge { path });
        } else if pull_all {
            actions.push(Action::Download { path });
        }
    }
    actions
}

/// Plan the push pass: uploads (new, edited, or still under an old key) and remote deletions.
pub fn plan_push(
    local: &[LocalFile],
    base: &[BaseEntry],
    remote: &[RemoteFile],
    current_key_id: &str,
) -> Vec<Action> {
    let ix = Index::new(local, base, remote);
    let mut actions = Vec::new();

    let local_paths: BTreeSet<&str> = ix.local.keys().copied().collect();
    for &path in &local_paths {
        let l = ix.local[path];
        let live_remote = ix.remote.get(path).filter(|r| !r.deleted);
        let needs_upload = match ix.remote.get(path) {
            None => true,
            Some(r) => {
                let stale_key = !r.deleted && !r.key_id.is_empty() && r.key_id != current_key_id;
                stale_key || (ix.locally_modified(path) && (r.deleted || r.hash != l.hash))
            }
        };
        if needs_upload {
            actions.push(Action::Upload {
                path: path.to_string(),
                expected_remote_hash: live_remote.map(|r| r.hash.clone()),
            });
        }
    }

    let synced: BTreeSet<&str> = ix.base.keys().copied().collect();
    for &path in synced.difference(&local_paths) {
        let edited_remotely = ix
            .remote
            .get(path)
            .is_some_and(|r| !r.deleted && ix.remotely_modified(r));
        actions.push(if edited_remotely {
            Action::RestoreRemote {
                path: path.to_string(),
            }
        } else {
            Action::DeleteRemote {
                path: path.to_string(),
            }
        });
    }
    actions
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ResolutionKind {
    /// One side was a superset of the other (or both were equal).
    Clean,
    /// Non-overlapping edits were combined.
    AutoMerged,
    /// Overlapping edits: the remote version wins the path and the local one is kept as a copy.
    ConflictCopy,
}

/// The outcome of a [`Action::Merge`]: write `content` to the path, write `conflict_copy` (path,
/// local content) if present, and record the remote version as the new base. The next push
/// uploads `content` if it differs from the remote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Resolution {
    pub kind: ResolutionKind,
    pub content: String,
    pub conflict_copy: Option<String>,
}

pub fn resolve(path: &str, base: &str, local: &str, remote: &str, now_ms: i64) -> Resolution {
    let outcome = if payload::is_binary(local) || payload::is_binary(remote) {
        if local == remote {
            MergeOutcome::Clean(local.to_string())
        } else {
            MergeOutcome::Conflict
        }
    } else {
        merge_three_way(base, local, remote)
    };
    match outcome {
        MergeOutcome::Clean(content) => Resolution {
            kind: ResolutionKind::Clean,
            content,
            conflict_copy: None,
        },
        MergeOutcome::AutoMerged(content) => Resolution {
            kind: ResolutionKind::AutoMerged,
            content,
            conflict_copy: None,
        },
        MergeOutcome::Conflict => Resolution {
            kind: ResolutionKind::ConflictCopy,
            content: remote.to_string(),
            conflict_copy: Some(conflict_copy_path(path, now_ms)),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn l(path: &str, hash: &str) -> LocalFile {
        LocalFile {
            path: path.into(),
            hash: hash.into(),
        }
    }
    fn b(path: &str, hash: &str) -> BaseEntry {
        BaseEntry {
            path: path.into(),
            remote_hash: hash.into(),
        }
    }
    fn r(path: &str, hash: &str) -> RemoteFile {
        RemoteFile {
            path: path.into(),
            hash: hash.into(),
            deleted: false,
            key_id: "k".into(),
        }
    }
    fn tomb(path: &str) -> RemoteFile {
        RemoteFile {
            deleted: true,
            hash: String::new(),
            ..r(path, "")
        }
    }
    fn up(path: &str, expected: Option<&str>) -> Action {
        Action::Upload {
            path: path.into(),
            expected_remote_hash: expected.map(str::to_string),
        }
    }

    #[test]
    fn in_sync_paths_need_nothing() {
        let local = [l("a.md", "1")];
        let base = [b("a.md", "1")];
        let remote = [r("a.md", "1")];
        assert!(plan_pull(&local, &base, &remote, true).is_empty());
        assert!(plan_push(&local, &base, &remote, "k").is_empty());
    }

    #[test]
    fn remote_only_change_downloads() {
        let plan = plan_pull(
            &[l("a.md", "1")],
            &[b("a.md", "1")],
            &[r("a.md", "2")],
            true,
        );
        assert_eq!(
            plan,
            [Action::Download {
                path: "a.md".into()
            }]
        );
    }

    #[test]
    fn new_remote_file_downloads() {
        let plan = plan_pull(&[], &[], &[r("new.md", "1")], true);
        assert_eq!(
            plan,
            [Action::Download {
                path: "new.md".into()
            }]
        );
    }

    #[test]
    fn both_changed_merges_even_in_push_only_mode() {
        let args = ([l("a.md", "L")], [b("a.md", "1")], [r("a.md", "R")]);
        let merge = [Action::Merge {
            path: "a.md".into(),
        }];
        assert_eq!(plan_pull(&args.0, &args.1, &args.2, true), merge);
        assert_eq!(plan_pull(&args.0, &args.1, &args.2, false), merge);
    }

    #[test]
    fn unsynced_local_and_remote_with_different_content_merge() {
        let plan = plan_pull(&[l("a.md", "L")], &[], &[r("a.md", "R")], true);
        assert_eq!(
            plan,
            [Action::Merge {
                path: "a.md".into()
            }]
        );
    }

    #[test]
    fn identical_content_adopts_base() {
        let plan = plan_pull(
            &[l("a.md", "2")],
            &[b("a.md", "1")],
            &[r("a.md", "2")],
            true,
        );
        assert_eq!(
            plan,
            [Action::AdoptBase {
                path: "a.md".into(),
                hash: "2".into()
            }]
        );
    }

    #[test]
    fn push_only_mode_skips_downloads_and_deletes() {
        let local = [l("a.md", "1"), l("gone.md", "1")];
        let base = [b("a.md", "1"), b("gone.md", "1")];
        let remote = [r("a.md", "2"), tomb("gone.md")];
        assert!(plan_pull(&local, &base, &remote, false).is_empty());
    }

    #[test]
    fn remote_delete_of_untouched_file_deletes_locally() {
        let plan = plan_pull(&[l("a.md", "1")], &[b("a.md", "1")], &[tomb("a.md")], true);
        assert_eq!(
            plan,
            [Action::DeleteLocal {
                path: "a.md".into()
            }]
        );
    }

    #[test]
    fn remote_delete_of_edited_file_keeps_it_and_reuploads() {
        let local = [l("a.md", "2")];
        let remote = [tomb("a.md")];
        let pull = plan_pull(&local, &[b("a.md", "1")], &remote, true);
        assert_eq!(
            pull,
            [Action::KeepLocalOverRemoteDelete {
                path: "a.md".into()
            }]
        );
        // After forgetting the base, the push uploads it over the tombstone.
        assert_eq!(plan_push(&local, &[], &remote, "k"), [up("a.md", None)]);
    }

    #[test]
    fn delete_on_both_sides_forgets_base() {
        let plan = plan_pull(&[], &[b("a.md", "1")], &[tomb("a.md")], true);
        assert_eq!(
            plan,
            [Action::ForgetBase {
                path: "a.md".into()
            }]
        );
        assert!(plan_pull(&[], &[], &[tomb("a.md")], true).is_empty());
    }

    #[test]
    fn local_edits_and_new_files_upload_with_expected_hash() {
        let local = [l("a.md", "2"), l("new.md", "1")];
        let plan = plan_push(&local, &[b("a.md", "1")], &[r("a.md", "1")], "k");
        assert_eq!(plan, [up("a.md", Some("1")), up("new.md", None)]);
    }

    #[test]
    fn untouched_file_never_overwrites_a_newer_remote() {
        // Push-only sync where pull was skipped: local is the old base, remote moved on.
        let plan = plan_push(&[l("a.md", "1")], &[b("a.md", "1")], &[r("a.md", "2")], "k");
        assert!(plan.is_empty());
    }

    #[test]
    fn missing_remote_row_reuploads() {
        let plan = plan_push(&[l("a.md", "1")], &[b("a.md", "1")], &[], "k");
        assert_eq!(plan, [up("a.md", None)]);
    }

    #[test]
    fn stale_key_reuploads_unchanged_content() {
        let mut remote = r("a.md", "1");
        remote.key_id = "old".into();
        let plan = plan_push(
            &[l("a.md", "1")],
            &[b("a.md", "1")],
            &[remote.clone()],
            "new",
        );
        assert_eq!(plan, [up("a.md", Some("1"))]);
        // Backends that don't record key ids never trigger re-uploads.
        remote.key_id = String::new();
        assert!(plan_push(&[l("a.md", "1")], &[b("a.md", "1")], &[remote], "new").is_empty());
    }

    #[test]
    fn local_delete_tombstones_or_restores() {
        let base = [b("a.md", "1"), b("b.md", "1"), b("c.md", "1")];
        let remote = [r("a.md", "1"), r("b.md", "2")];
        let plan = plan_push(&[], &base, &remote, "k");
        assert_eq!(
            plan,
            [
                Action::DeleteRemote {
                    path: "a.md".into()
                },
                Action::RestoreRemote {
                    path: "b.md".into()
                },
                Action::DeleteRemote {
                    path: "c.md".into()
                },
            ]
        );
    }

    #[test]
    fn reserved_paths_are_never_planned() {
        let keyring = crate::crypto::keyring::KEYRING_PATH;
        let remote = [r(keyring, "1")];
        assert!(plan_pull(&[], &[], &remote, true).is_empty());
        assert!(plan_push(&[l(keyring, "1")], &[], &remote, "k").is_empty());
    }

    #[test]
    fn plans_are_sorted_by_path() {
        let remote = [r("z.md", "1"), r("a.md", "1"), r("m.md", "1")];
        let paths: Vec<_> = plan_pull(&[], &[], &remote, true)
            .into_iter()
            .map(|a| match a {
                Action::Download { path } => path,
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(paths, ["a.md", "m.md", "z.md"]);
    }

    #[test]
    fn resolve_outcomes() {
        let base = "a\nb\nc\n";
        let clean = resolve("n.md", base, base, "a\nb\nc\nd\n", 0);
        assert_eq!(clean.kind, ResolutionKind::Clean);
        assert_eq!(clean.content, "a\nb\nc\nd\n");

        let merged = resolve("n.md", base, "A\nb\nc\n", "a\nb\nC\n", 0);
        assert_eq!(merged.kind, ResolutionKind::AutoMerged);
        assert_eq!(merged.content, "A\nb\nC\n");

        let conflict = resolve("n.md", base, "X\nb\nc\n", "Y\nb\nc\n", 1_700_000_000_000);
        assert_eq!(conflict.kind, ResolutionKind::ConflictCopy);
        assert_eq!(conflict.content, "Y\nb\nc\n");
        assert_eq!(
            conflict.conflict_copy.as_deref(),
            Some("n.conflict-1700000000.md")
        );

        let bin_a = payload::encode_binary(b"a");
        let bin_b = payload::encode_binary(b"b");
        assert_eq!(
            resolve("i.png", "", &bin_a, &bin_a, 0).kind,
            ResolutionKind::Clean
        );
        assert_eq!(
            resolve("i.png", "", &bin_a, &bin_b, 0).kind,
            ResolutionKind::ConflictCopy
        );
    }

    #[test]
    fn actions_serialize_for_the_plugin() {
        let json = serde_json::to_string(&up("a.md", Some("1"))).unwrap();
        assert_eq!(
            json,
            r#"{"action":"upload","path":"a.md","expectedRemoteHash":"1"}"#
        );
        let back: Action = serde_json::from_str(&json).unwrap();
        assert_eq!(back, up("a.md", Some("1")));
    }
}
