//! The pure half of a Claude Desktop account switch: deciding which session
//! indexes to copy, what the merged schedule registry looks like, and what the
//! rewritten `config.json` / `ant-device-registry.json` contain.
//!
//! Nothing here touches `$HOME`, spawns a process, or writes a file — every
//! function takes an explicit root or the file's bytes, which is what lets
//! `--dry-run` be free (compute the plan, print it, stop) and lets the whole
//! algorithm be unit-tested against a temporary directory on any platform.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::error::{AppError, Result};

/// Per-account schedule registry, stored beside that account's session indexes.
const SCHEDULED_TASKS: &str = "scheduled-tasks.json";

/// Session-index files that a switch would bring into the target account's
/// history folder, so it shows the union of everything rather than only the
/// conversations that account happened to start.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SessionMerge {
    /// `(source, destination)` for indexes the target has never seen.
    pub copied: Vec<(PathBuf, PathBuf)>,
    /// `(source, destination)` where the source is strictly newer. A resumed
    /// chat's index advances, so a stale copy in the target folder *must* be
    /// overwritten — otherwise the app reopens it at an old resume point and
    /// the recent messages look like they vanished.
    pub updated: Vec<(PathBuf, PathBuf)>,
}

impl SessionMerge {
    pub fn is_empty(&self) -> bool {
        self.copied.is_empty() && self.updated.is_empty()
    }
}

/// The merged `scheduled-tasks.json` for the target account, rendered but not
/// yet written.
#[derive(Debug, PartialEq, Eq)]
pub struct ScheduledMerge {
    pub target: PathBuf,
    pub bytes: Vec<u8>,
    /// Tasks the target did not already have.
    pub added: usize,
}

/// Plan the session-index merge into `<sessions_root>/<account_uuid>/<org_uuid>`.
///
/// Infallible: an unreadable or absent tree simply means there is nothing to
/// merge. Only the *writes* can fail, and those abort the switch before it
/// touches the app or any credential.
pub fn plan_session_merge(
    sessions_root: &Path,
    account_uuid: &str,
    org_uuid: &str,
) -> SessionMerge {
    let target_dir = sessions_root.join(account_uuid).join(org_uuid);

    // Keyed by destination so two source accounts holding the same index can't
    // both queue a write — the newest one wins, exactly as it would if the
    // copies were applied one at a time.
    let mut best: BTreeMap<PathBuf, (PathBuf, i64)> = BTreeMap::new();
    for source_dir in account_org_dirs(sessions_root) {
        if source_dir == target_dir {
            continue;
        }
        for source in local_session_files(&source_dir) {
            let Some(name) = source.file_name() else {
                continue;
            };
            let destination = target_dir.join(name);
            let activity = last_activity(&source);
            match best.get(&destination) {
                Some((_, seen)) if *seen >= activity => {}
                _ => {
                    best.insert(destination, (source, activity));
                }
            }
        }
    }

    let mut merge = SessionMerge::default();
    for (destination, (source, activity)) in best {
        if !destination.exists() {
            merge.copied.push((source, destination));
        } else if activity > last_activity(&destination) {
            merge.updated.push((source, destination));
        }
    }
    merge
}

/// Plan the routine/schedule merge: a union by task `id`, with the newer
/// `createdAt` winning a conflict. The task *scripts* these point at live in
/// `~/.claude/scheduled-tasks/<name>/` and are global already, so only the
/// account-scoped registry needs merging.
pub fn plan_scheduled_merge(
    sessions_root: &Path,
    account_uuid: &str,
    org_uuid: &str,
) -> Result<ScheduledMerge> {
    let target = sessions_root
        .join(account_uuid)
        .join(org_uuid)
        .join(SCHEDULED_TASKS);
    let (target_tasks, mut skips) = load_scheduled(&target);

    // A Vec rather than a map: registries hold a handful of entries, and the
    // target's own tasks must keep their existing order.
    let mut tasks: Vec<(String, Value)> = target_tasks
        .into_iter()
        .filter_map(|task| task_id(&task).map(|id| (id, task)))
        .collect();
    let mut added = 0usize;

    for source in scheduled_task_files(sessions_root) {
        if source == target {
            continue;
        }
        let (source_tasks, source_skips) = load_scheduled(&source);
        for task in source_tasks {
            let Some(id) = task_id(&task) else {
                continue;
            };
            match tasks.iter_mut().find(|(existing, _)| *existing == id) {
                Some((_, existing)) => {
                    if created_at(&task) > created_at(existing) {
                        *existing = task;
                    }
                }
                None => {
                    tasks.push((id, task));
                    added += 1;
                }
            }
        }
        for (key, value) in source_skips {
            // First recorded skip wins; a later account's copy never overrides.
            skips.entry(key).or_insert(value);
        }
    }

    let merged = serde_json::json!({
        "scheduledTasks": tasks.into_iter().map(|(_, task)| task).collect::<Vec<_>>(),
        "recordedSkips": skips,
    });
    Ok(ScheduledMerge {
        target,
        bytes: serde_json::to_vec(&merged)?,
        added,
    })
}

/// Rewrite Claude Desktop's `config.json` so the app comes back as a different
/// account: both OAuth token-cache blobs plus the `lastKnownAccountUuid`
/// pointer that tells the app who it is. Every other key — the dxt allowlists,
/// window state, feature flags — is carried through untouched.
pub fn swap_config_tokens(
    existing: &[u8],
    token_cache: &str,
    token_cache_v2: &str,
    account_uuid: &str,
) -> Result<Vec<u8>> {
    let mut document: Value = serde_json::from_slice(existing)?;
    let object = document
        .as_object_mut()
        .ok_or_else(|| AppError::Other("Claude Desktop config.json is not a JSON object".into()))?;
    object.insert("oauth:tokenCache".into(), token_cache.into());
    object.insert("oauth:tokenCacheV2".into(), token_cache_v2.into());
    object.insert("lastKnownAccountUuid".into(), account_uuid.into());
    Ok(serde_json::to_vec(&document)?)
}

/// Strip everything the Desktop app uses to remember an account, so it reopens
/// at the login screen. Only the identity keys go: the dxt allowlists, window
/// state and feature flags are the user's settings, not their session.
pub fn clear_config_tokens(existing: &[u8]) -> Result<Vec<u8>> {
    let mut document: Value = serde_json::from_slice(existing)?;
    let object = document
        .as_object_mut()
        .ok_or_else(|| AppError::Other("Claude Desktop config.json is not a JSON object".into()))?;
    for key in [
        "oauth:tokenCache",
        "oauth:tokenCacheV2",
        "lastKnownAccountUuid",
    ] {
        object.remove(key);
    }
    Ok(serde_json::to_vec(&document)?)
}

/// The account the app has finished signing in as, or `None` while the login
/// is still in progress. Both fields matter: `lastKnownAccountUuid` appears
/// before the token cache is written, so keying on it alone captures a
/// half-finished login.
pub fn logged_in_account(config: &[u8]) -> Option<String> {
    let document: Value = serde_json::from_slice(config).ok()?;
    let has_both_tokens = ["oauth:tokenCache", "oauth:tokenCacheV2"]
        .into_iter()
        .all(|key| {
            document
                .get(key)
                .and_then(Value::as_str)
                .is_some_and(|token| !token.is_empty())
        });
    if !has_both_tokens {
        return None;
    }
    document
        .get("lastKnownAccountUuid")?
        .as_str()
        .filter(|uuid| !uuid.is_empty())
        .map(str::to_string)
}

/// An organisation this machine has not seen before, recovered from the dxt
/// allowlist keys the app writes per org (`dxt:allowlistEnabled:<org>`). The
/// fallback for when the account's session folder has not appeared yet.
pub fn new_org_in_config(config: &[u8], known: &[String]) -> Option<String> {
    let mut unseen = orgs_in_config(config)
        .into_iter()
        .filter(|org| !known.contains(org));
    let candidate = unseen.next()?;
    unseen.next().is_none().then_some(candidate)
}

/// Every organisation already represented by a dxt allowlist key. Capture
/// records this baseline before clearing the login, so an old unmanaged
/// account's key cannot be mistaken for the new account's organisation.
pub fn orgs_in_config(config: &[u8]) -> Vec<String> {
    const PREFIX: &str = "dxt:allowlistEnabled:";
    let Ok(document) = serde_json::from_slice::<Value>(config) else {
        return Vec::new();
    };
    let Some(object) = document.as_object() else {
        return Vec::new();
    };
    let mut orgs: Vec<String> = object
        .keys()
        .filter_map(|key| key.strip_prefix(PREFIX))
        .filter(|org| !org.is_empty())
        .map(str::to_string)
        .collect();
    orgs.sort();
    orgs.dedup();
    orgs
}

/// Fold a per-profile snapshot of `ant-device-registry.json` back into the live
/// one. The file is a map of account UUID → device registration and already
/// holds every account, so this is purely additive: **the live value wins every
/// conflict**, and the only thing a snapshot can contribute is a key the live
/// file somehow lost. That makes it impossible for this step to be the cause of
/// a broken browser pairing, which is the whole point of doing it at all.
///
/// A malformed snapshot returns the live file unchanged.
pub fn merge_device_registry(live: &[u8], snapshot: &[u8]) -> Result<Vec<u8>> {
    let mut merged: Map<String, Value> = serde_json::from_slice(live)?;
    if let Ok(saved) = serde_json::from_slice::<Map<String, Value>>(snapshot) {
        for (account_uuid, registration) in saved {
            merged.entry(account_uuid).or_insert(registration);
        }
    }
    Ok(serde_json::to_vec(&merged)?)
}

/// Every `<account>/<org>` directory under the session root.
fn account_org_dirs(sessions_root: &Path) -> Vec<PathBuf> {
    let Ok(accounts) = std::fs::read_dir(sessions_root) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for account in accounts.flatten() {
        let Ok(orgs) = std::fs::read_dir(account.path()) else {
            continue;
        };
        out.extend(orgs.flatten().map(|org| org.path()).filter(|p| p.is_dir()));
    }
    out.sort();
    out
}

/// `local_*.json` only — never an account-level file such as
/// `scheduled-tasks.json`, which lives in the same folder but is merged by id.
fn local_session_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("local_") && name.ends_with(".json"))
        })
        .collect();
    out.sort();
    out
}

fn scheduled_task_files(sessions_root: &Path) -> Vec<PathBuf> {
    account_org_dirs(sessions_root)
        .into_iter()
        .map(|dir| dir.join(SCHEDULED_TASKS))
        .filter(|path| path.is_file())
        .collect()
}

fn load_scheduled(path: &Path) -> (Vec<Value>, Map<String, Value>) {
    let Ok(bytes) = std::fs::read(path) else {
        return (Vec::new(), Map::new());
    };
    let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
        return (Vec::new(), Map::new());
    };
    let tasks = value
        .get("scheduledTasks")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let skips = value
        .get("recordedSkips")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    (tasks, skips)
}

fn task_id(task: &Value) -> Option<String> {
    task.get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

fn created_at(task: &Value) -> i64 {
    task.get("createdAt").map_or(0, json_i64)
}

fn last_activity(path: &Path) -> i64 {
    let Ok(bytes) = std::fs::read(path) else {
        return 0;
    };
    serde_json::from_slice::<Value>(&bytes)
        .ok()
        .and_then(|value| value.get("lastActivityAt").map(json_i64))
        .unwrap_or(0)
}

/// Timestamps arrive as JSON numbers, but a schema-tolerant read costs one line
/// and a wrong `0` would silently make every merge decision "keep the target".
fn json_i64(value: &Value) -> i64 {
    value
        .as_i64()
        .or_else(|| value.as_f64().map(|seconds| seconds as i64))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    fn session(activity: i64) -> String {
        format!("{{\"lastActivityAt\":{activity},\"cwd\":\"/tmp/demo\"}}")
    }

    #[test]
    fn session_merge_takes_the_newest_copy() {
        let root = tempfile::TempDir::new().unwrap();
        let sessions = root.path();
        write(&sessions.join("A/O1/local_x.json"), &session(100));
        write(&sessions.join("B/O2/local_x.json"), &session(200));

        let merge = plan_session_merge(sessions, "A", "O1");
        assert!(merge.copied.is_empty());
        assert_eq!(merge.updated.len(), 1);
        assert_eq!(merge.updated[0].0, sessions.join("B/O2/local_x.json"));
        assert_eq!(merge.updated[0].1, sessions.join("A/O1/local_x.json"));
    }

    #[test]
    fn session_merge_keeps_a_newer_target() {
        let root = tempfile::TempDir::new().unwrap();
        let sessions = root.path();
        write(&sessions.join("A/O1/local_x.json"), &session(300));
        write(&sessions.join("B/O2/local_x.json"), &session(200));

        assert_eq!(
            plan_session_merge(sessions, "A", "O1"),
            SessionMerge::default()
        );
    }

    #[test]
    fn session_merge_copies_indexes_the_target_lacks() {
        let root = tempfile::TempDir::new().unwrap();
        let sessions = root.path();
        write(&sessions.join("A/O1/local_x.json"), &session(100));
        write(&sessions.join("B/O2/local_y.json"), &session(50));

        let merge = plan_session_merge(sessions, "A", "O1");
        assert_eq!(merge.updated.len(), 0);
        assert_eq!(merge.copied.len(), 1);
        assert_eq!(merge.copied[0].1, sessions.join("A/O1/local_y.json"));
    }

    #[test]
    fn session_merge_prefers_the_newest_of_several_sources() {
        let root = tempfile::TempDir::new().unwrap();
        let sessions = root.path();
        write(&sessions.join("A/O1/local_x.json"), &session(10));
        write(&sessions.join("B/O2/local_x.json"), &session(200));
        write(&sessions.join("C/O3/local_x.json"), &session(150));

        let merge = plan_session_merge(sessions, "A", "O1");
        assert_eq!(merge.updated.len(), 1);
        assert_eq!(merge.updated[0].0, sessions.join("B/O2/local_x.json"));
    }

    #[test]
    fn session_merge_never_touches_the_schedule_registry() {
        let root = tempfile::TempDir::new().unwrap();
        let sessions = root.path();
        write(&sessions.join("A/O1/local_x.json"), &session(100));
        write(&sessions.join("B/O2/scheduled-tasks.json"), "{}");

        let merge = plan_session_merge(sessions, "A", "O1");
        assert!(merge.is_empty(), "{merge:?}");
    }

    #[test]
    fn scheduled_merge_unions_by_id_with_the_newer_winning() {
        let root = tempfile::TempDir::new().unwrap();
        let sessions = root.path();
        write(
            &sessions.join("A/O1/scheduled-tasks.json"),
            r#"{"scheduledTasks":[{"id":"t1","createdAt":100,"name":"old"}],
                "recordedSkips":{"t1":"target"}}"#,
        );
        write(
            &sessions.join("B/O2/scheduled-tasks.json"),
            r#"{"scheduledTasks":[{"id":"t1","createdAt":200,"name":"new"},
                                  {"id":"t2","createdAt":5,"name":"extra"}],
                "recordedSkips":{"t1":"other","t2":"other"}}"#,
        );

        let merge = plan_scheduled_merge(sessions, "A", "O1").unwrap();
        assert_eq!(merge.added, 1);
        let value: Value = serde_json::from_slice(&merge.bytes).unwrap();
        let tasks = value["scheduledTasks"].as_array().unwrap();
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0]["name"], "new", "newer createdAt must win");
        assert_eq!(tasks[1]["id"], "t2");
        // setdefault semantics: the target's own skip is never overridden.
        assert_eq!(value["recordedSkips"]["t1"], "target");
        assert_eq!(value["recordedSkips"]["t2"], "other");
    }

    #[test]
    fn scheduled_merge_keeps_the_target_when_it_is_newer() {
        let root = tempfile::TempDir::new().unwrap();
        let sessions = root.path();
        write(
            &sessions.join("A/O1/scheduled-tasks.json"),
            r#"{"scheduledTasks":[{"id":"t1","createdAt":300,"name":"target"}]}"#,
        );
        write(
            &sessions.join("B/O2/scheduled-tasks.json"),
            r#"{"scheduledTasks":[{"id":"t1","createdAt":100,"name":"other"}]}"#,
        );

        let merge = plan_scheduled_merge(sessions, "A", "O1").unwrap();
        assert_eq!(merge.added, 0);
        let value: Value = serde_json::from_slice(&merge.bytes).unwrap();
        assert_eq!(value["scheduledTasks"][0]["name"], "target");
    }

    #[test]
    fn config_swap_preserves_every_unrelated_key() {
        let existing = br#"{"lastKnownAccountUuid":"old","oauth:tokenCache":"a",
            "oauth:tokenCacheV2":"b","dxt:allowlistEnabled:org-1":true,
            "windowBounds":{"width":1200}}"#;

        let bytes = swap_config_tokens(existing, "new-a", "new-b", "new-uuid").unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["oauth:tokenCache"], "new-a");
        assert_eq!(value["oauth:tokenCacheV2"], "new-b");
        assert_eq!(value["lastKnownAccountUuid"], "new-uuid");
        assert_eq!(value["dxt:allowlistEnabled:org-1"], true);
        assert_eq!(value["windowBounds"]["width"], 1200);
    }

    #[test]
    fn config_swap_rejects_a_non_object_document() {
        assert!(swap_config_tokens(b"[]", "a", "b", "u").is_err());
    }

    #[test]
    fn clearing_tokens_keeps_the_users_settings() {
        let existing = br#"{"lastKnownAccountUuid":"u","oauth:tokenCache":"a",
            "oauth:tokenCacheV2":"b","dxt:allowlistEnabled:org-1":true,"autoUpdates":true}"#;

        let bytes = clear_config_tokens(existing).unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(value.get("oauth:tokenCache").is_none());
        assert!(value.get("oauth:tokenCacheV2").is_none());
        assert!(value.get("lastKnownAccountUuid").is_none());
        assert_eq!(value["dxt:allowlistEnabled:org-1"], true);
        assert_eq!(value["autoUpdates"], true);
    }

    #[test]
    fn a_login_counts_only_once_both_fields_are_written() {
        assert_eq!(
            logged_in_account(&clear_config_tokens(b"{}").unwrap()),
            None
        );
        // The uuid lands before the token cache does; capturing here would
        // save a profile with no credential in it.
        assert_eq!(logged_in_account(br#"{"lastKnownAccountUuid":"u"}"#), None);
        assert_eq!(
            logged_in_account(br#"{"lastKnownAccountUuid":"u","oauth:tokenCacheV2":""}"#),
            None
        );
        assert_eq!(
            logged_in_account(br#"{"lastKnownAccountUuid":"u","oauth:tokenCacheV2":"t"}"#),
            None
        );
        assert_eq!(
            logged_in_account(
                br#"{"lastKnownAccountUuid":"u","oauth:tokenCache":"a","oauth:tokenCacheV2":"b"}"#
            ),
            Some("u".into())
        );
        assert_eq!(logged_in_account(b"not json"), None);
    }

    #[test]
    fn a_new_org_is_recovered_from_the_allowlist_keys() {
        let config = br#"{"dxt:allowlistEnabled:org-old":true,
            "dxt:allowlistEnabled:org-new":true,"unrelated":1}"#;

        assert_eq!(
            new_org_in_config(config, &["org-old".into()]),
            Some("org-new".into())
        );
        assert_eq!(
            new_org_in_config(config, &["org-old".into(), "org-new".into()]),
            None
        );
        assert_eq!(new_org_in_config(b"{}", &[]), None);
        assert_eq!(
            new_org_in_config(
                br#"{"dxt:allowlistEnabled:org-a":true,"dxt:allowlistEnabled:org-b":true}"#,
                &[]
            ),
            None,
            "multiple new orgs are ambiguous"
        );
        assert_eq!(
            orgs_in_config(config),
            ["org-new".to_string(), "org-old".to_string()]
        );
    }

    #[test]
    fn device_registry_merge_lets_the_live_value_win() {
        let live = br#"{"acct-a":{"deviceId":"live-a"},"acct-b":{"deviceId":"live-b"}}"#;
        let snapshot = br#"{"acct-b":{"deviceId":"stale-b"},"acct-c":{"deviceId":"saved-c"}}"#;

        let bytes = merge_device_registry(live, snapshot).unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["acct-a"]["deviceId"], "live-a");
        assert_eq!(value["acct-b"]["deviceId"], "live-b", "live must win");
        assert_eq!(value["acct-c"]["deviceId"], "saved-c");
    }

    #[test]
    fn device_registry_merge_ignores_a_malformed_snapshot() {
        let live = br#"{"acct-a":{"deviceId":"live-a"}}"#;
        let bytes = merge_device_registry(live, b"not json at all").unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["acct-a"]["deviceId"], "live-a");
        assert_eq!(value.as_object().unwrap().len(), 1);
    }
}
