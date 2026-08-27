//! Locate the Command Code OAuth credential a local harness already holds.
//!
//! Command Code is reachable through several agent harnesses, and each files
//! the same OAuth credential under its own key in its own file. This module
//! reads them; it never writes. Refreshing an expired token belongs to the
//! CLI that owns the file, and racing it here would corrupt state shared by
//! every harness on the machine.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::cache::home_dir;
use crate::error::{AppError, Result};

/// Files searched in order; the first holding a live credential wins.
pub const AUTH_FILES: &[&str] = &[
    ".commandcode/auth.json", // official command-code CLI
    ".omp/agent/auth.json",
    ".pi/agent/auth.json",
];

/// Keys a harness may file the credential under.
const CREDENTIAL_KEYS: &[&str] = &["command-code", "commandcode"];

pub const SIGNED_OUT: &str =
    "Command Code is not signed in. Run `commandcode` and sign in, or set COMMANDCODE_API_KEY.";
pub const EXPIRED: &str = "Command Code sign-in expired. Run `commandcode` to sign in again.";

/// A credential and where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credential {
    pub token: String,
    pub source: String,
}

/// One candidate found on disk, before expiry is considered.
struct Candidate {
    token: String,
    expires_ms: Option<i64>,
}

/// Default search paths, resolved against the OS home convention.
pub fn default_paths() -> Result<Vec<PathBuf>> {
    let home = home_dir()?;
    Ok(AUTH_FILES.iter().map(|rel| home.join(rel)).collect())
}

/// Read whichever credential shape a harness wrote into `path`.
///
/// A missing or malformed file yields `None` rather than an error: one broken
/// harness must not mask a working one further down the list.
pub fn read_from(path: &Path) -> Option<Credential> {
    let text = std::fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    let object = value.as_object()?;

    let candidates = CREDENTIAL_KEYS
        .iter()
        .filter_map(|key| object.get(*key))
        .chain(object.get("apiKey"))
        .filter_map(candidate);

    for found in candidates {
        if found.expires_ms.is_some_and(is_past) {
            continue;
        }
        return Some(Credential {
            token: found.token,
            source: path.display().to_string(),
        });
    }
    None
}

/// Production resolver: the env override, then configured paths, then the
/// platform defaults. Tests use [`resolve_from`], which takes both as inputs.
pub fn resolve(configured: Option<&[PathBuf]>) -> Result<Credential> {
    let paths = match configured {
        Some(paths) if !paths.is_empty() => paths.to_vec(),
        _ => default_paths()?,
    };
    resolve_from(std::env::var("COMMANDCODE_API_KEY").ok().as_deref(), &paths)
}

/// Resolve one credential from the environment, then each path in turn.
pub fn resolve_from(env_key: Option<&str>, paths: &[PathBuf]) -> Result<Credential> {
    if let Some(key) = env_key.map(str::trim).filter(|key| !key.is_empty()) {
        return Ok(Credential {
            token: key.to_string(),
            source: "COMMANDCODE_API_KEY".to_string(),
        });
    }
    for path in paths {
        if let Some(credential) = read_from(path) {
            return Ok(credential);
        }
    }
    Err(AppError::Credentials(message_for(paths)))
}

/// Distinguish "never signed in" from "signed in, but the token lapsed" so the
/// tooltip can tell the user which one to fix.
fn message_for(paths: &[PathBuf]) -> String {
    let any_expired = paths.iter().any(|path| {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .and_then(|value| {
                let object = value.as_object()?.clone();
                let found = CREDENTIAL_KEYS
                    .iter()
                    .filter_map(|key| object.get(*key))
                    .chain(object.get("apiKey"))
                    .filter_map(candidate)
                    .any(|found| found.expires_ms.is_some_and(is_past));
                Some(found)
            })
            .unwrap_or(false)
    });
    if any_expired { EXPIRED } else { SIGNED_OUT }.to_string()
}

fn candidate(value: &Value) -> Option<Candidate> {
    if let Some(token) = value.as_str() {
        let token = token.trim();
        return (!token.is_empty()).then(|| Candidate {
            token: token.to_string(),
            expires_ms: None,
        });
    }
    let object = value.as_object()?;
    let token = ["access", "apiKey", "key"]
        .iter()
        .filter_map(|field| object.get(*field))
        .filter_map(Value::as_str)
        .map(str::trim)
        .find(|token| !token.is_empty())?;
    Some(Candidate {
        token: token.to_string(),
        expires_ms: object.get("expires").and_then(Value::as_i64),
    })
}

fn is_past(expires_ms: i64) -> bool {
    expires_ms > 0 && expires_ms <= chrono::Utc::now().timestamp_millis()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, value: Value) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        path
    }

    fn future_ms() -> i64 {
        chrono::Utc::now().timestamp_millis() + 86_400_000
    }

    fn past_ms() -> i64 {
        chrono::Utc::now().timestamp_millis() - 86_400_000
    }

    #[test]
    fn reads_the_official_cli_oauth_shape() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "auth.json",
            serde_json::json!({
                "command-code": {"type": "oauth", "access": "tok", "expires": future_ms()}
            }),
        );

        assert_eq!(read_from(&path).unwrap().token, "tok");
    }

    #[test]
    fn reads_the_pi_and_omp_oauth_shape() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "auth.json",
            serde_json::json!({
                "openai-codex": {"type": "oauth", "access": "other"},
                "commandcode": {"type": "oauth", "access": "tok", "expires": future_ms()}
            }),
        );

        assert_eq!(read_from(&path).unwrap().token, "tok");
    }

    #[test]
    fn reads_a_legacy_plain_string_credential() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "auth.json",
            serde_json::json!({"commandcode": "plain-token"}),
        );

        assert_eq!(read_from(&path).unwrap().token, "plain-token");
    }

    #[test]
    fn an_expired_token_is_not_offered() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "auth.json",
            serde_json::json!({
                "command-code": {"type": "oauth", "access": "tok", "expires": past_ms()}
            }),
        );

        assert!(read_from(&path).is_none());
    }

    #[test]
    fn malformed_and_missing_files_are_skipped_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let broken = dir.path().join("broken.json");
        std::fs::write(&broken, "{not json").unwrap();

        assert!(read_from(&broken).is_none());
        assert!(read_from(&dir.path().join("absent.json")).is_none());
    }

    #[test]
    fn env_key_outranks_every_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "auth.json",
            serde_json::json!({"commandcode": {"access": "from-file"}}),
        );

        let credential = resolve_from(Some("from-env"), &[path]).unwrap();

        assert_eq!(credential.token, "from-env");
        assert_eq!(credential.source, "COMMANDCODE_API_KEY");
    }

    #[test]
    fn a_stale_harness_does_not_mask_a_live_one() {
        let dir = tempfile::tempdir().unwrap();
        let stale = write(
            dir.path(),
            "stale.json",
            serde_json::json!({
                "command-code": {"access": "stale", "expires": past_ms()}
            }),
        );
        let live = write(
            dir.path(),
            "live.json",
            serde_json::json!({
                "commandcode": {"access": "live", "expires": future_ms()}
            }),
        );

        assert_eq!(resolve_from(None, &[stale, live]).unwrap().token, "live");
    }

    #[test]
    fn all_expired_reports_expiry_and_nothing_reports_signed_out() {
        let dir = tempfile::tempdir().unwrap();
        let stale = write(
            dir.path(),
            "stale.json",
            serde_json::json!({
                "command-code": {"access": "stale", "expires": past_ms()}
            }),
        );

        let expired = resolve_from(None, std::slice::from_ref(&stale)).unwrap_err();
        assert!(expired.to_string().contains("expired"), "{expired}");

        let absent = resolve_from(None, &[dir.path().join("absent.json")]).unwrap_err();
        assert!(absent.to_string().contains("not signed in"), "{absent}");
    }

    #[test]
    fn default_paths_cover_every_known_harness() {
        let paths = default_paths().unwrap();

        assert_eq!(paths.len(), AUTH_FILES.len());
        assert!(paths[0].ends_with(".commandcode/auth.json"));
    }
}
