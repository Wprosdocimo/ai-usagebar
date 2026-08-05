//! Read and write `~/.grok/auth.json` — the OIDC state the Grok Build CLI
//! maintains after `grok login`.
//!
//! Wire shape (confirmed against a live SuperGrok install of the official
//! CLI): a JSON object keyed by `"<issuer>::<client_id>"`, each value an
//! account entry with `key` (access token), `refresh_token`, `expires_at`,
//! `user_id`, and `oidc_client_id`. Unknown fields are preserved on write so
//! we never strip CLI-owned metadata.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use base64::Engine;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::cache::atomic_write;
use crate::error::{AppError, Result};

use super::oauth;

/// One account entry under the Grok CLI auth map.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AccountEntry {
    /// Access token (JWT). Field name is `key` in the CLI's on-disk format.
    pub key: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_at: Option<String>,
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub oidc_client_id: Option<String>,
    #[serde(default)]
    pub oidc_issuer: Option<String>,
    #[serde(default)]
    pub auth_mode: Option<String>,
    /// Preserve every unknown field the CLI wrote.
    #[serde(flatten, default)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// Top-level auth file: map of account-slot key → entry.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AuthFile {
    #[serde(flatten)]
    pub accounts: BTreeMap<String, AccountEntry>,
}

/// Default location: `~/.grok/auth.json` (Unix/macOS) or
/// `%USERPROFILE%\.grok\auth.json` (Windows), via [`crate::cache::home_dir`].
pub fn default_path() -> Result<PathBuf> {
    Ok(crate::cache::home_dir()?.join(".grok").join("auth.json"))
}

pub fn read_from(path: &Path) -> Result<AuthFile> {
    let raw = std::fs::read_to_string(path).map_err(|e| AppError::io_at(path, e))?;
    let file: AuthFile = serde_json::from_str(&raw).map_err(|e| {
        AppError::Credentials(format!(
            "could not parse {}: {e}. Run `grok login` to re-authenticate.",
            path.display()
        ))
    })?;
    if file.accounts.is_empty() {
        return Err(AppError::Credentials(format!(
            "no SuperGrok accounts in {}. Run `grok login`.",
            path.display()
        )));
    }
    Ok(file)
}

/// Persist the whole file, preserving unknown fields. Atomic.
pub fn write_back(path: &Path, auth: &AuthFile) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(auth).map_err(AppError::Json)?;
    atomic_write(path, &bytes)
}

/// Pick the best account entry: prefer an OIDC slot with both access and
/// refresh tokens. Returns `(slot_key, entry)`.
pub fn select_account(auth: &AuthFile) -> Result<(String, AccountEntry)> {
    // Prefer entries that can silently refresh.
    if let Some((k, v)) = auth.accounts.iter().find(|(_, e)| {
        !e.key.trim().is_empty()
            && e.refresh_token
                .as_deref()
                .is_some_and(|r| !r.trim().is_empty())
    }) {
        return Ok((k.clone(), v.clone()));
    }
    // Fall back to any non-empty access token (user can still see usage until
    // it expires, then get a clear re-login error).
    if let Some((k, v)) = auth.accounts.iter().find(|(_, e)| !e.key.trim().is_empty()) {
        return Ok((k.clone(), v.clone()));
    }
    Err(AppError::Credentials(
        "SuperGrok: auth.json has no usable access token. Run `grok login`.".into(),
    ))
}

impl AccountEntry {
    /// Stable account fingerprint for cache isolation — never displayed.
    pub fn account_key(&self) -> String {
        self.user_id
            .as_deref()
            .filter(|s| !s.is_empty())
            .or(self.email.as_deref().filter(|s| !s.is_empty()))
            .unwrap_or("default")
            .to_string()
    }

    /// Client id for the OIDC refresh grant. Prefer the explicit field; fall
    /// back to parsing the auth-map slot key (`issuer::client_id`).
    pub fn client_id(&self, slot_key: &str) -> Result<String> {
        if let Some(c) = self
            .oidc_client_id
            .as_deref()
            .filter(|s| !s.trim().is_empty())
        {
            return Ok(c.to_string());
        }
        if let Some((_, client)) = slot_key.rsplit_once("::")
            && !client.trim().is_empty()
        {
            return Ok(client.to_string());
        }
        Err(AppError::Credentials(
            "SuperGrok: auth entry has no oidc_client_id — run `grok login`.".into(),
        ))
    }

    /// Unix-seconds expiry. Explicit `expires_at` wins; else the access-token
    /// JWT `exp` claim; else `0` (force refresh).
    pub fn expires_at_secs(&self) -> i64 {
        self.expires_at
            .as_deref()
            .and_then(parse_expires_at)
            .or_else(|| parse_jwt_exp(&self.key))
            .unwrap_or(0)
    }

    pub fn apply_refresh(
        &mut self,
        access_token: String,
        refresh_token: Option<String>,
        expires_in: Option<u64>,
        now: DateTime<Utc>,
    ) {
        self.key = access_token;
        if let Some(rt) = refresh_token.filter(|s| !s.trim().is_empty()) {
            self.refresh_token = Some(rt);
        }
        let secs = expires_in.unwrap_or(21_600) as i64;
        let exp = now + chrono::Duration::seconds(secs.max(0));
        self.expires_at = Some(exp.to_rfc3339());
    }
}

fn parse_expires_at(s: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.timestamp())
        .or_else(|| {
            // Grok CLI has been observed writing fractional seconds with a
            // trailing `Z` that chrono's strict RFC3339 rejects in some
            // versions — try a light normalization.
            let normalized = if let Some(stripped) = s.strip_suffix('Z') {
                format!("{stripped}+00:00")
            } else {
                s.to_string()
            };
            // Truncate >6 fractional digits if present.
            let normalized = truncate_frac(&normalized);
            DateTime::parse_from_rfc3339(&normalized)
                .ok()
                .map(|dt| dt.timestamp())
        })
}

fn truncate_frac(s: &str) -> String {
    // "...SSS.fffffffff+00:00" or "...SSS.fffffffffZ-style already normalized"
    let Some(dot) = s.find('.') else {
        return s.to_string();
    };
    let rest = &s[dot + 1..];
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.len() <= 6 {
        return s.to_string();
    }
    let after = &rest[digits.len()..];
    format!("{}.{}{}", &s[..dot], &digits[..6], after)
}

fn parse_jwt_exp(token: &str) -> Option<i64> {
    let claims = parse_jwt_claims(token)?;
    claims
        .get("exp")
        .and_then(|v| v.as_i64())
        .or_else(|| claims.get("exp").and_then(|v| v.as_f64()).map(|f| f as i64))
}

fn parse_jwt_claims(token: &str) -> Option<serde_json::Value> {
    let mut parts = token.split('.');
    let _header = parts.next()?;
    let payload = parts.next()?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(payload))
        .ok()?;
    serde_json::from_slice(&decoded).ok()
}

/// Convenience: does this entry need a silent refresh at `now`?
pub fn needs_refresh(entry: &AccountEntry, now: DateTime<Utc>) -> bool {
    oauth::needs_refresh(entry.expires_at_secs(), now.timestamp())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::{NamedTempFile, TempDir};

    fn sample_json() -> &'static str {
        r#"{
          "https://auth.x.ai::client-abc": {
            "key": "access-token",
            "auth_mode": "oidc",
            "refresh_token": "refresh-token",
            "expires_at": "2026-08-05T20:00:00Z",
            "user_id": "user-1",
            "email": "a@b.c",
            "oidc_client_id": "client-abc",
            "oidc_issuer": "https://auth.x.ai",
            "extra_cli_field": true
          }
        }"#
    }

    #[test]
    fn reads_and_selects_oidc_account() {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(sample_json().as_bytes()).unwrap();
        let auth = read_from(f.path()).unwrap();
        let (slot, entry) = select_account(&auth).unwrap();
        assert!(slot.contains("auth.x.ai"));
        assert_eq!(entry.key, "access-token");
        assert_eq!(entry.refresh_token.as_deref(), Some("refresh-token"));
        assert_eq!(entry.account_key(), "user-1");
        assert_eq!(entry.client_id(&slot).unwrap(), "client-abc");
        // Unknown field preserved for write-back.
        assert_eq!(
            entry.extra.get("extra_cli_field"),
            Some(&serde_json::json!(true))
        );
    }

    #[test]
    fn write_back_preserves_unknown_fields() {
        let td = TempDir::new().unwrap();
        let path = td.path().join("auth.json");
        let auth = read_from({
            let mut f = NamedTempFile::new().unwrap();
            f.write_all(sample_json().as_bytes()).unwrap();
            // copy into td so we can write back
            std::fs::write(&path, sample_json()).unwrap();
            &path
        })
        .unwrap();
        write_back(&path, &auth).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("extra_cli_field"));
        assert!(raw.contains("refresh-token"));
    }

    #[test]
    fn fractional_expires_at_with_z_parses() {
        let entry = AccountEntry {
            key: "x".into(),
            refresh_token: None,
            expires_at: Some("2026-08-05T20:38:23.675722453Z".into()),
            user_id: None,
            email: None,
            oidc_client_id: None,
            oidc_issuer: None,
            auth_mode: None,
            extra: Default::default(),
        };
        assert!(entry.expires_at_secs() > 0);
    }

    #[test]
    fn empty_file_is_credentials_error() {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(b"{}").unwrap();
        assert!(read_from(f.path()).is_err());
    }

    #[test]
    fn client_id_falls_back_to_slot_key() {
        let entry = AccountEntry {
            key: "x".into(),
            refresh_token: Some("r".into()),
            expires_at: None,
            user_id: None,
            email: None,
            oidc_client_id: None,
            oidc_issuer: None,
            auth_mode: None,
            extra: Default::default(),
        };
        assert_eq!(
            entry.client_id("https://auth.x.ai::from-slot").unwrap(),
            "from-slot"
        );
    }
}
