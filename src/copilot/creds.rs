//! Persist the GitHub OAuth token `ai-usagebar login copilot` obtains.
//!
//! Unlike every other local-file vendor here (Cursor, Kiro), Copilot has no
//! existing CLI/IDE credential file to read — GitHub gates `copilot_internal/*`
//! by which OAuth App issued the token, and neither the `gh` CLI's own token
//! nor a personal access token qualifies (confirmed live: both get a 403
//! "scraping" response). So this is the one vendor where ai-usagebar performs
//! its own device-code login (`device_flow.rs`) and keeps the resulting
//! token — a **classic GitHub OAuth token that does not expire** on its own
//! (revocation by the user is the only normal invalidation path) — in its own
//! file, chmod 600 like the Settings overlay does for inline API keys.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::cache::atomic_write;
use crate::error::{AppError, Result};

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Credentials {
    pub access_token: String,
}

/// `<config_dir>/copilot-credentials.json`, alongside `config.toml` — not
/// under `~/.cache`, since a login credential isn't disposable the way a
/// fetch cache is.
pub fn default_path() -> Result<PathBuf> {
    let proj = directories::ProjectDirs::from("", "", "ai-usagebar").ok_or_else(|| {
        AppError::Other("could not resolve the platform config directory (no HOME?)".into())
    })?;
    Ok(proj.config_dir().join("copilot-credentials.json"))
}

pub fn read_from(path: &Path) -> Result<Credentials> {
    if !path.exists() {
        return Err(AppError::Credentials(format!(
            "no Copilot credentials at {}. Run `ai-usagebar login copilot`, then try again.",
            path.display()
        )));
    }
    let raw = std::fs::read_to_string(path).map_err(|e| AppError::io_at(path, e))?;
    let creds: Credentials = serde_json::from_str(&raw).map_err(|e| {
        AppError::Credentials(format!(
            "could not parse {}: {e}. Run `ai-usagebar login copilot` again.",
            path.display()
        ))
    })?;
    if creds.access_token.trim().is_empty() {
        return Err(AppError::Credentials(
            "Copilot credentials file has an empty access_token. Run `ai-usagebar login copilot` again."
                .into(),
        ));
    }
    Ok(creds)
}

/// Atomic write + best-effort private permissions (`0600` on Unix; a no-op
/// elsewhere, mirroring `claude_desktop::app::set_private_mode`).
pub fn write_to(path: &Path, creds: &Credentials) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(creds).map_err(AppError::Json)?;
    atomic_write(path, &bytes)?;
    set_private_mode(path)
}

#[cfg(unix)]
fn set_private_mode(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| AppError::io_at(path, e))
}

#[cfg(not(unix))]
fn set_private_mode(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn default_path_ends_with_copilot_credentials_json() {
        let p = default_path().unwrap();
        assert!(p.ends_with(std::path::Path::new("copilot-credentials.json")));
    }

    #[test]
    fn missing_file_is_a_credentials_error_naming_the_path() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("copilot-credentials.json");
        let err = read_from(&path).unwrap_err();
        match err {
            AppError::Credentials(m) => assert!(m.contains(&path.display().to_string())),
            other => panic!("expected Credentials error, got {other:?}"),
        }
    }

    #[test]
    fn round_trips_through_write_and_read() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("sub").join("copilot-credentials.json");
        let creds = Credentials {
            access_token: "gho_example".into(),
        };
        write_to(&path, &creds).unwrap();
        assert_eq!(read_from(&path).unwrap(), creds);
    }

    #[test]
    fn malformed_json_is_a_credentials_error() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("copilot-credentials.json");
        std::fs::write(&path, "not json").unwrap();
        assert!(matches!(read_from(&path), Err(AppError::Credentials(_))));
    }

    #[test]
    fn empty_token_is_a_credentials_error() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("copilot-credentials.json");
        write_to(
            &path,
            &Credentials {
                access_token: "".into(),
            },
        )
        .unwrap();
        assert!(matches!(read_from(&path), Err(AppError::Credentials(_))));
    }

    #[cfg(unix)]
    #[test]
    fn written_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("copilot-credentials.json");
        write_to(
            &path,
            &Credentials {
                access_token: "gho_example".into(),
            },
        )
        .unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
