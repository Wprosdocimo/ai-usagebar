//! Read (and refresh-write-back) the **Claude Desktop app's own** OAuth token so
//! its usage shows up with no `claude` CLI login at all.
//!
//! The Desktop app stores its token under the same public OAuth client id as
//! Claude Code (`oauth::CLIENT_ID`) — proven by the token being accepted at the
//! usage endpoint — so once we lift it into an [`OauthCreds`] the entire
//! existing fetch/refresh/cache/render path works unchanged. The only new work
//! is getting it out of, and back into, the encrypted [`crate::safe_storage`]
//! blob.
//!
//! Two blob locations, same encryption and inner shape:
//!   - the live `config.json` (`oauth:tokenCacheV2` / `oauth:tokenCache` values)
//!     — the *active* account, kept fresh by the running app;
//!   - a claude-acc profile snapshot (`config-tokenCacheV2` / `config-tokenCache`
//!     files, each the bare base64 value) — every *other* saved account.
//!
//! Decrypted, each is a JSON object keyed by `<clientId>:<org>:<aud>:<scopes>`;
//! only the entry whose scopes include `user:inference` can call the usage API,
//! so that is the one we lift.
//!
//! **Refresh safety.** Refreshing rotates the refresh token and invalidates the
//! old one, so it must never run against a token the Desktop app is actively
//! using. The active account is therefore constructed read-only *while the app
//! runs* (`allow_refresh = false`), which blanks the refresh token so the
//! generic fetch path never refreshes it — it just uses the app-maintained
//! access token or falls back to cache. Non-active snapshots (and the active
//! account when the app is stopped) refresh freely and write the rotation back
//! to the same blob they came from, keeping the snapshot switch-ready.

use std::path::PathBuf;

use serde_json::Value;

use crate::error::{AppError, Result};
use crate::safe_storage;

use super::creds::{CredentialsFile, OauthCreds};

/// Only this scope can hit the usage endpoint; the app also stores a
/// profile-only token we must skip.
const INFERENCE_SCOPE: &str = "user:inference";

/// Config.json keys / snapshot file names, newest format first.
const CONFIG_KEYS: [&str; 2] = ["oauth:tokenCacheV2", "oauth:tokenCache"];
const SNAPSHOT_FILES: [&str; 2] = ["config-tokenCacheV2", "config-tokenCache"];

/// Where a Desktop token blob lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlobSource {
    /// The live `config.json`; the blob is a string value under one of
    /// [`CONFIG_KEYS`], with the rest of the file preserved on write-back.
    ConfigJson(PathBuf),
    /// A claude-acc profile directory; each of [`SNAPSHOT_FILES`] is the bare
    /// base64 value.
    ProfileDir(PathBuf),
}

/// A resolved Desktop credential source. Cloneable plain data so it can ride
/// inside [`super::creds::CredsTarget`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopCreds {
    pub source: BlobSource,
    pub key: [u8; 16],
    /// False for the active account while the app runs — blanks the refresh
    /// token so nothing refreshes the app's live credential out from under it.
    pub allow_refresh: bool,
}

/// Enough to put a refreshed token back exactly where it was read from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Writeback {
    source: BlobSource,
    key: [u8; 16],
    /// Which config key / snapshot file the value was read from.
    slot: String,
    /// The `<clientId>:…:<scopes>` entry within the decrypted object.
    entry_key: String,
    allow_refresh: bool,
}

/// Resolve a saved Desktop account label to a fetchable target + its cache.
/// Shared by the TUI and the widget so both surface the same accounts. macOS
/// only — the Desktop token store and its Keychain key exist nowhere else, and
/// such accounts are never enumerated off-Mac.
#[cfg(target_os = "macos")]
pub fn account_target(
    config: &crate::config::Config,
    label: &str,
) -> Result<(super::creds::CredsTarget, crate::cache::Cache)> {
    let paths = crate::claude_desktop::Paths::resolve(&config.anthropic)?;
    let profile = crate::claude_desktop::load_profiles(&paths.profiles_dir)
        .into_iter()
        .find(|p| p.label == label)
        .ok_or_else(|| AppError::Other(format!("no saved Desktop profile {label:?}")))?;
    let is_active = crate::claude_desktop::active_account_uuid(&paths.config_json())
        .is_some_and(|uuid| uuid == profile.account_uuid);
    let key = crate::safe_storage::macos_key()?;
    let source = source_for(
        &paths.config_json(),
        &paths.profile_dir(label),
        is_active,
        crate::claude_desktop::app::is_running(),
        key,
    );
    // Plain `anthropic/<label>` cache — one usage source per label — so the
    // widget, menu bar and `claude-acc list` all read the same file.
    let cache = crate::cache::Cache::for_vendor_account("anthropic", label)?;
    Ok((super::creds::CredsTarget::Desktop(source), cache))
}

#[cfg(not(target_os = "macos"))]
pub fn account_target(
    _config: &crate::config::Config,
    label: &str,
) -> Result<(super::creds::CredsTarget, crate::cache::Cache)> {
    Err(AppError::Other(format!(
        "Claude Desktop accounts are macOS-only (requested {label:?})"
    )))
}

/// Build the credential source for a saved Desktop profile, applying the
/// refresh-safety policy. The *active* account (the one `config.json` currently
/// points at) is read from the live `config.json` — kept fresh by the app — and
/// is refreshable only while the app is stopped. Every other account is read
/// from its own snapshot and refreshes freely. Pure: the key and the app-running
/// flag are injected, so it is testable without a Keychain or a running app.
pub fn source_for(
    config_json: &std::path::Path,
    profile_dir: &std::path::Path,
    is_active: bool,
    app_running: bool,
    key: [u8; 16],
) -> DesktopCreds {
    if is_active {
        DesktopCreds {
            source: BlobSource::ConfigJson(config_json.to_path_buf()),
            key,
            allow_refresh: !app_running,
        }
    } else {
        DesktopCreds {
            source: BlobSource::ProfileDir(profile_dir.to_path_buf()),
            key,
            allow_refresh: true,
        }
    }
}

impl DesktopCreds {
    /// The file this source decrypts from, for diagnostics only.
    pub fn blob_path(&self) -> &std::path::Path {
        match &self.source {
            BlobSource::ConfigJson(p) | BlobSource::ProfileDir(p) => p,
        }
    }

    /// Lift the inference token into [`OauthCreds`], plus a [`Writeback`] handle
    /// aimed at the same slot. `Err` if the blob is absent, undecryptable, or
    /// has no inference entry — the caller drops the account from the report.
    pub fn read(&self) -> Result<(CredentialsFile, Writeback)> {
        let (slot, blob) = self.first_present_blob()?;
        let plain = safe_storage::decrypt(&self.key, &blob)?;
        let obj: Value = serde_json::from_slice(&plain)
            .map_err(|e| AppError::Other(format!("Desktop token cache is not JSON: {e}")))?;
        let map = obj
            .as_object()
            .ok_or_else(|| AppError::Other("Desktop token cache is not a JSON object".into()))?;

        let (entry_key, entry) = map
            .iter()
            .find(|(k, _)| k.contains(INFERENCE_SCOPE))
            .ok_or_else(|| {
                AppError::Other("no inference-scoped token in the Desktop cache".into())
            })?;

        let oauth = entry_to_oauth(entry, self.allow_refresh)?;
        let writeback = Writeback {
            source: self.source.clone(),
            key: self.key,
            slot,
            entry_key: entry_key.clone(),
            allow_refresh: self.allow_refresh,
        };
        Ok((
            CredentialsFile {
                claude_ai_oauth: oauth,
            },
            writeback,
        ))
    }

    /// The first present blob (base64 value) and the slot it came from.
    fn first_present_blob(&self) -> Result<(String, String)> {
        match &self.source {
            BlobSource::ConfigJson(path) => {
                let bytes = std::fs::read(path).map_err(|e| AppError::io_at(path, e))?;
                let cfg: Value = serde_json::from_slice(&bytes)
                    .map_err(|e| AppError::Other(format!("config.json is not JSON: {e}")))?;
                for key in CONFIG_KEYS {
                    if let Some(v) = cfg.get(key).and_then(Value::as_str) {
                        return Ok((key.to_string(), v.to_string()));
                    }
                }
                Err(AppError::Other(
                    "config.json has no oauth token cache".into(),
                ))
            }
            BlobSource::ProfileDir(dir) => {
                for file in SNAPSHOT_FILES {
                    let path = dir.join(file);
                    if let Ok(v) = std::fs::read_to_string(&path) {
                        return Ok((file.to_string(), v));
                    }
                }
                Err(AppError::Other(format!(
                    "no token cache snapshot in {}",
                    dir.display()
                )))
            }
        }
    }
}

impl Writeback {
    /// Persist a refreshed token back into the same blob it was read from.
    /// A read-only source (active account, app running) is a no-op — but the
    /// fetch path never refreshes such a source, so this is only defence.
    pub fn write(&self, oauth: &OauthCreds) -> Result<()> {
        if !self.allow_refresh {
            return Ok(());
        }
        let blob = self.read_slot_blob()?;
        let plain = safe_storage::decrypt(&self.key, &blob)?;
        let mut obj: Value = serde_json::from_slice(&plain)
            .map_err(|e| AppError::Other(format!("Desktop token cache is not JSON: {e}")))?;
        let entry = obj
            .as_object_mut()
            .and_then(|m| m.get_mut(&self.entry_key))
            .and_then(Value::as_object_mut)
            .ok_or_else(|| AppError::Other("token entry vanished before write-back".into()))?;
        entry.insert("token".into(), Value::String(oauth.access_token.clone()));
        entry.insert(
            "refreshToken".into(),
            Value::String(oauth.refresh_token.clone()),
        );
        entry.insert("expiresAt".into(), Value::from(oauth.expires_at_ms));

        let new_blob = safe_storage::encrypt(&self.key, &serde_json::to_vec(&obj)?);
        self.write_slot_blob(&new_blob)
    }

    fn read_slot_blob(&self) -> Result<String> {
        match &self.source {
            BlobSource::ConfigJson(path) => {
                let bytes = std::fs::read(path).map_err(|e| AppError::io_at(path, e))?;
                let cfg: Value = serde_json::from_slice(&bytes)
                    .map_err(|e| AppError::Other(format!("config.json is not JSON: {e}")))?;
                cfg.get(&self.slot)
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .ok_or_else(|| AppError::Other("config token slot vanished".into()))
            }
            BlobSource::ProfileDir(dir) => {
                let path = dir.join(&self.slot);
                std::fs::read_to_string(&path).map_err(|e| AppError::io_at(&path, e))
            }
        }
    }

    fn write_slot_blob(&self, new_blob: &str) -> Result<()> {
        match &self.source {
            BlobSource::ConfigJson(path) => {
                // Read-modify-write: only the one token value changes; every
                // other config.json field (lastKnownAccountUuid, the other
                // cache variant, app settings) is preserved.
                let bytes = std::fs::read(path).map_err(|e| AppError::io_at(path, e))?;
                let mut cfg: Value = serde_json::from_slice(&bytes)
                    .map_err(|e| AppError::Other(format!("config.json is not JSON: {e}")))?;
                cfg.as_object_mut()
                    .ok_or_else(|| AppError::Other("config.json is not an object".into()))?
                    .insert(self.slot.clone(), Value::String(new_blob.to_string()));
                crate::cache::atomic_write(path, &serde_json::to_vec_pretty(&cfg)?)
            }
            BlobSource::ProfileDir(dir) => {
                crate::cache::atomic_write(&dir.join(&self.slot), new_blob.as_bytes())
            }
        }
    }
}

/// Map one decrypted `{token, refreshToken, expiresAt, …}` entry to [`OauthCreds`].
fn entry_to_oauth(entry: &Value, allow_refresh: bool) -> Result<OauthCreds> {
    let get_str = |k: &str| {
        entry
            .get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let access_token = get_str("token");
    if access_token.is_empty() {
        return Err(AppError::Other("Desktop token entry has no `token`".into()));
    }
    Ok(OauthCreds {
        access_token,
        // Blanked when read-only: `creds::can_refresh` is then false, so the
        // fetch path leaves the app's live credential untouched.
        refresh_token: if allow_refresh {
            get_str("refreshToken")
        } else {
            String::new()
        },
        expires_at_ms: entry.get("expiresAt").and_then(Value::as_i64).unwrap_or(0),
        subscription_type: get_str("subscriptionType"),
        rate_limit_tier: get_str("rateLimitTier"),
        scopes: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> [u8; 16] {
        safe_storage::derive_key(b"test-secret")
    }

    // A decrypted token-cache object with the inference entry plus a
    // profile-only decoy the picker must skip.
    fn cache_json(access: &str, refresh: &str, expires: i64) -> Vec<u8> {
        let obj = serde_json::json!({
            "9d1c250a:org:https://api.anthropic.com:user:inference user:profile": {
                "token": access,
                "refreshToken": refresh,
                "expiresAt": expires,
                "subscriptionType": "max",
                "rateLimitTier": "default_claude_max_20x",
            },
            "a473d7bb:org:https://api.anthropic.com:user:profile": {
                "token": "sk-ant-oat01-PROFILE-ONLY",
                "refreshToken": "sk-ant-ort01-PROFILE-ONLY",
                "expiresAt": expires,
                "subscriptionType": "max",
                "rateLimitTier": "default_claude_max_20x",
            }
        });
        serde_json::to_vec(&obj).unwrap()
    }

    fn profile_with(dir: &std::path::Path, plain: &[u8], k: &[u8; 16]) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("config-tokenCacheV2"),
            safe_storage::encrypt(k, plain),
        )
        .unwrap();
    }

    #[test]
    fn lifts_the_inference_token_from_a_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let k = key();
        profile_with(
            tmp.path(),
            &cache_json("sk-ant-oat01-REAL", "sk-ant-ort01-REAL", 111),
            &k,
        );

        let d = DesktopCreds {
            source: BlobSource::ProfileDir(tmp.path().to_path_buf()),
            key: k,
            allow_refresh: true,
        };
        let (creds, _wb) = d.read().unwrap();
        let o = creds.claude_ai_oauth;
        // The inference entry, not the profile-only decoy.
        assert_eq!(o.access_token, "sk-ant-oat01-REAL");
        assert_eq!(o.refresh_token, "sk-ant-ort01-REAL");
        assert_eq!(o.expires_at_ms, 111);
        assert_eq!(o.plan_label(), "Max 20x");
    }

    #[test]
    fn read_only_blanks_the_refresh_token() {
        // Active-account-while-running policy: no refresh token surfaces, so the
        // generic fetch path can't rotate the app's live credential.
        let tmp = tempfile::tempdir().unwrap();
        let k = key();
        profile_with(tmp.path(), &cache_json("at", "rt", 1), &k);
        let d = DesktopCreds {
            source: BlobSource::ProfileDir(tmp.path().to_path_buf()),
            key: k,
            allow_refresh: false,
        };
        let (creds, wb) = d.read().unwrap();
        assert_eq!(creds.claude_ai_oauth.refresh_token, "");
        // And a write-back is a no-op even if called.
        wb.write(&creds.claude_ai_oauth).unwrap();
    }

    #[test]
    fn writeback_rotates_only_the_token_fields_in_place() {
        let tmp = tempfile::tempdir().unwrap();
        let k = key();
        profile_with(tmp.path(), &cache_json("old-at", "old-rt", 1), &k);
        let d = DesktopCreds {
            source: BlobSource::ProfileDir(tmp.path().to_path_buf()),
            key: k,
            allow_refresh: true,
        };
        let (mut creds, wb) = d.read().unwrap();
        creds.claude_ai_oauth.access_token = "new-at".into();
        creds.claude_ai_oauth.refresh_token = "new-rt".into();
        creds.claude_ai_oauth.expires_at_ms = 999;
        wb.write(&creds.claude_ai_oauth).unwrap();

        // Re-read: rotation landed, and the profile-only decoy is untouched.
        let (creds2, _) = d.read().unwrap();
        assert_eq!(creds2.claude_ai_oauth.access_token, "new-at");
        assert_eq!(creds2.claude_ai_oauth.expires_at_ms, 999);
        let plain = safe_storage::decrypt(
            &k,
            &std::fs::read_to_string(tmp.path().join("config-tokenCacheV2")).unwrap(),
        )
        .unwrap();
        let obj: Value = serde_json::from_slice(&plain).unwrap();
        assert_eq!(
            obj["a473d7bb:org:https://api.anthropic.com:user:profile"]["token"],
            "sk-ant-oat01-PROFILE-ONLY"
        );
    }

    #[test]
    fn active_account_reads_from_config_json_and_preserves_other_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let k = key();
        let cfg = tmp.path().join("config.json");
        let blob = safe_storage::encrypt(&k, &cache_json("cfg-at", "cfg-rt", 5));
        std::fs::write(
            &cfg,
            serde_json::to_vec(&serde_json::json!({
                "lastKnownAccountUuid": "keep-me",
                "oauth:tokenCacheV2": blob,
            }))
            .unwrap(),
        )
        .unwrap();

        let d = DesktopCreds {
            source: BlobSource::ConfigJson(cfg.clone()),
            key: k,
            allow_refresh: true,
        };
        let (mut creds, wb) = d.read().unwrap();
        assert_eq!(creds.claude_ai_oauth.access_token, "cfg-at");
        creds.claude_ai_oauth.access_token = "rotated".into();
        wb.write(&creds.claude_ai_oauth).unwrap();

        let after: Value = serde_json::from_slice(&std::fs::read(&cfg).unwrap()).unwrap();
        // Sibling field survived the write-back.
        assert_eq!(after["lastKnownAccountUuid"], "keep-me");
        let (creds2, _) = d.read().unwrap();
        assert_eq!(creds2.claude_ai_oauth.access_token, "rotated");
    }

    #[test]
    fn missing_blob_is_an_error_not_a_panic() {
        let tmp = tempfile::tempdir().unwrap();
        let d = DesktopCreds {
            source: BlobSource::ProfileDir(tmp.path().to_path_buf()),
            key: key(),
            allow_refresh: true,
        };
        assert!(d.read().is_err());
    }
}
