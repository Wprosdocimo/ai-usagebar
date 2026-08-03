//! Orchestrates a Copilot snapshot: read the GitHub OAuth token
//! `ai-usagebar login copilot` stored (`creds.rs`), then call
//! `GET api.github.com/copilot_internal/user`. Cache/stale/error-fallback
//! shape mirrors `cursor::fetch`. No refresh step — see `creds.rs` for why.

use std::path::Path;
use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::cache::{Cache, MAX_STALE, acquire_lock_async};
use crate::error::{AppError, Result};
use crate::usage::CopilotSnapshot;
use crate::vendor::{MAX_BODY_BYTES, read_body_capped};

use super::creds;
use super::types::{self, UserResponse};

pub const USER_URL: &str = "https://api.github.com/copilot_internal/user";
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
const LOCK_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone)]
pub struct Endpoints {
    pub user: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            user: USER_URL.into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct FetchOutcome {
    pub snapshot: CopilotSnapshot,
    pub stale: bool,
    pub last_error: Option<(u16, String)>,
    pub cache_age: Option<Duration>,
}

/// Cache-aware fetch. `creds_path` is where `ai-usagebar login copilot`
/// wrote the GitHub token — the caller resolves `[copilot] credentials_path`
/// (config override) vs [`creds::default_path`], the same override pattern
/// as `cursor.db_path`.
pub async fn fetch_snapshot(
    client: &reqwest::Client,
    creds_path: &Path,
    cache: &Cache,
    endpoints: &Endpoints,
    cache_ttl: Duration,
) -> Result<FetchOutcome> {
    fetch_snapshot_at(client, creds_path, cache, endpoints, cache_ttl, Utc::now()).await
}

/// Clock seam for cache rollover tests, mirroring `cursor::fetch::fetch_snapshot_at`.
async fn fetch_snapshot_at(
    client: &reqwest::Client,
    creds_path: &Path,
    cache: &Cache,
    endpoints: &Endpoints,
    cache_ttl: Duration,
    now: DateTime<Utc>,
) -> Result<FetchOutcome> {
    cache.ensure_dir()?;
    let _lock = acquire_lock_async(&cache.lock_path(), LOCK_TIMEOUT).await?;

    let token = creds::read_from(creds_path)?.access_token;

    if let Some(bytes) = cache.fresh_payload(cache_ttl)?
        && let Ok(outcome) = reuse_cache(&bytes, cache, false, now)
    {
        return Ok(outcome);
    }

    match fetch_live(client, endpoints, &token).await {
        Ok(snap) => {
            let bytes = serde_json::to_vec(&snap_to_json(&snap))?;
            cache.write_payload(&bytes)?;
            Ok(FetchOutcome {
                snapshot: snap,
                stale: false,
                last_error: None,
                cache_age: Some(Duration::ZERO),
            })
        }
        Err(e) if e.is_transient() => fallback_silent(cache, now, e),
        Err(e) => {
            cache.mark_stale();
            if let Some((code, msg)) = error_to_pair(&e) {
                cache.write_last_error(code, &msg);
            }
            fallback_with_error(cache, now, e)
        }
    }
}

fn fallback_silent(cache: &Cache, now: DateTime<Utc>, original: AppError) -> Result<FetchOutcome> {
    let Some(bytes) = cache.fallback_payload(MAX_STALE)? else {
        return Err(original);
    };
    match reuse_cache(&bytes, cache, true, now) {
        Ok(outcome) => Ok(outcome),
        Err(_) => Err(original),
    }
}

fn fallback_with_error(
    cache: &Cache,
    now: DateTime<Utc>,
    original: AppError,
) -> Result<FetchOutcome> {
    let Some(bytes) = cache.fallback_payload(MAX_STALE)? else {
        return Err(original);
    };
    match reuse_cache(&bytes, cache, true, now) {
        Ok(mut outcome) => {
            outcome.last_error = error_to_pair(&original);
            Ok(outcome)
        }
        Err(_) => Err(original),
    }
}

/// Never surface upstream bodies for auth failures. Mirrors
/// `cursor::fetch::error_to_pair`.
fn error_to_pair(e: &AppError) -> Option<(u16, String)> {
    match e {
        AppError::Http { status, .. } if matches!(status, 401 | 403) => {
            Some((*status, "GitHub Copilot authentication failed".into()))
        }
        AppError::Http { status, body } => Some((*status, body.clone())),
        AppError::Credentials(msg) => Some((0, msg.clone())),
        e => Some((0, e.to_string())),
    }
}

fn reuse_cache(
    bytes: &[u8],
    cache: &Cache,
    stale: bool,
    now: DateTime<Utc>,
) -> Result<FetchOutcome> {
    let snap = parse_cache_at(bytes, now)?;
    Ok(FetchOutcome {
        snapshot: snap,
        stale,
        last_error: cache.read_last_error(),
        cache_age: cache.payload_age(),
    })
}

fn parse_cache_at(bytes: &[u8], now: DateTime<Utc>) -> Result<CopilotSnapshot> {
    let v: serde_json::Value = serde_json::from_slice(bytes)?;
    let entitlement = v["entitlement"]
        .as_f64()
        .filter(|n| n.is_finite() && *n >= 0.0)
        .ok_or_else(|| AppError::Schema("copilot cache: invalid entitlement".into()))?;
    let remaining = v["remaining"]
        .as_f64()
        .filter(|n| n.is_finite() && *n >= 0.0)
        .ok_or_else(|| AppError::Schema("copilot cache: invalid remaining".into()))?;
    let unlimited = v["unlimited"]
        .as_bool()
        .ok_or_else(|| AppError::Schema("copilot cache: invalid unlimited flag".into()))?;
    let reset_at = match &v["reset_at"] {
        serde_json::Value::Null => None,
        serde_json::Value::String(s) => Some(
            DateTime::parse_from_rfc3339(s)
                .map_err(|e| AppError::Schema(format!("copilot cache: invalid reset_at: {e}")))?
                .with_timezone(&Utc),
        ),
        _ => return Err(AppError::Schema("copilot cache: invalid reset_at".into())),
    };
    // Only reject a *known-past* reset — a missing reset is a legitimate shape.
    if let Some(reset_at) = reset_at
        && reset_at <= now
    {
        return Err(AppError::Schema(
            "copilot cache is past its quota-cycle reset; refetching".into(),
        ));
    }
    Ok(CopilotSnapshot {
        entitlement,
        remaining,
        unlimited,
        reset_at,
    })
}

fn snap_to_json(snap: &CopilotSnapshot) -> serde_json::Value {
    serde_json::json!({
        "entitlement": snap.entitlement,
        "remaining": snap.remaining,
        "unlimited": snap.unlimited,
        "reset_at": snap.reset_at.map(|dt| dt.to_rfc3339()),
    })
}

async fn fetch_live(
    client: &reqwest::Client,
    endpoints: &Endpoints,
    token: &str,
) -> Result<CopilotSnapshot> {
    let resp = tokio::time::timeout(
        HTTP_TIMEOUT,
        client
            .get(&endpoints.user)
            .header("Authorization", format!("Bearer {token}"))
            .header("Accept", "application/json")
            .header(
                "User-Agent",
                concat!("ai-usagebar/", env!("CARGO_PKG_VERSION")),
            )
            .send(),
    )
    .await
    .map_err(|_| AppError::Transport(format!("copilot timeout: {}", endpoints.user)))??;

    let status = resp.status();
    if !status.is_success() {
        let body = if matches!(status.as_u16(), 401 | 403) {
            "GitHub Copilot authentication failed".into()
        } else {
            format!("GitHub Copilot API returned HTTP {}", status.as_u16())
        };
        return Err(AppError::Http {
            status: status.as_u16(),
            body,
        });
    }

    let bytes = read_body_capped(resp, MAX_BODY_BYTES).await?;
    let parsed: UserResponse = serde_json::from_slice(&bytes)
        .map_err(|e| AppError::Schema(format!("copilot user response: {e}")))?;
    types::to_snapshot(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn cache_fixture() -> (TempDir, Cache) {
        let td = TempDir::new().unwrap();
        let cache = Cache::at(td.path().join("copilot"));
        cache.ensure_dir().unwrap();
        (td, cache)
    }

    fn creds_fixture(token: &str) -> (TempDir, std::path::PathBuf) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("copilot-credentials.json");
        creds::write_to(
            &path,
            &creds::Credentials {
                access_token: token.into(),
            },
        )
        .unwrap();
        (dir, path)
    }

    fn user_json() -> String {
        r#"{
            "quota_reset_date": "2099-01-01",
            "quota_snapshots": {
                "premium_interactions": { "entitlement": 300, "remaining": 250, "unlimited": false }
            }
        }"#
        .to_string()
    }

    #[tokio::test]
    async fn live_fetch_reads_the_token_and_calls_the_user_endpoint() {
        let mut server = mockito::Server::new_async().await;
        let m = server
            .mock("GET", "/copilot_internal/user")
            .match_header("authorization", "Bearer gho_example")
            .with_status(200)
            .with_body(user_json())
            .create_async()
            .await;

        let (_creds_dir, creds_path) = creds_fixture("gho_example");
        let (_cache_dir, cache) = cache_fixture();
        let client = reqwest::Client::new();
        let endpoints = Endpoints {
            user: format!("{}/copilot_internal/user", server.url()),
        };

        let out = fetch_snapshot(
            &client,
            &creds_path,
            &cache,
            &endpoints,
            Duration::from_secs(60),
        )
        .await
        .unwrap();

        assert_eq!(out.snapshot.entitlement, 300.0);
        assert_eq!(out.snapshot.remaining, 250.0);
        assert!(!out.stale);
        m.assert_async().await;
    }

    #[tokio::test]
    async fn missing_credentials_is_a_credentials_error() {
        let dir = TempDir::new().unwrap();
        let creds_path = dir.path().join("missing.json");
        let (_cache_dir, cache) = cache_fixture();
        let client = reqwest::Client::new();
        let err = fetch_snapshot(
            &client,
            &creds_path,
            &cache,
            &Endpoints::default(),
            Duration::from_secs(60),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, AppError::Credentials(_)));
    }

    #[tokio::test]
    async fn a_401_is_masked_and_falls_back_to_cache() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/copilot_internal/user")
            .with_status(401)
            .with_body(r#"{"message":"Bad credentials"}"#)
            .create_async()
            .await;

        let (_creds_dir, creds_path) = creds_fixture("gho_example");
        let (_cache_dir, cache) = cache_fixture();
        cache
            .write_payload(
                serde_json::to_vec(&serde_json::json!({
                    "entitlement": 300.0, "remaining": 100.0,
                    "unlimited": false, "reset_at": null,
                }))
                .unwrap()
                .as_slice(),
            )
            .unwrap();

        let client = reqwest::Client::new();
        let endpoints = Endpoints {
            user: format!("{}/copilot_internal/user", server.url()),
        };
        let out = fetch_snapshot_at(
            &client,
            &creds_path,
            &cache,
            &endpoints,
            Duration::from_millis(1),
            Utc::now(),
        )
        .await
        .unwrap();

        assert!(out.stale);
        assert_eq!(out.snapshot.remaining, 100.0);
        let (code, msg) = out.last_error.unwrap();
        assert_eq!(code, 401);
        assert!(!msg.contains("Bad credentials"));
    }

    #[tokio::test]
    async fn fresh_cache_is_served_without_a_network_call() {
        let (_creds_dir, creds_path) = creds_fixture("gho_example");
        let (_cache_dir, cache) = cache_fixture();
        cache
            .write_payload(
                serde_json::to_vec(&serde_json::json!({
                    "entitlement": 300.0, "remaining": 100.0,
                    "unlimited": false, "reset_at": null,
                }))
                .unwrap()
                .as_slice(),
            )
            .unwrap();

        let client = reqwest::Client::new();
        // No mock server configured for this endpoint — a fresh-cache hit
        // must never reach the network.
        let endpoints = Endpoints {
            user: "http://127.0.0.1:1/copilot_internal/user".into(),
        };
        let out = fetch_snapshot(
            &client,
            &creds_path,
            &cache,
            &endpoints,
            Duration::from_secs(60),
        )
        .await
        .unwrap();

        assert!(!out.stale);
        assert_eq!(out.snapshot.remaining, 100.0);
    }

    /// The quota cycle rolled over while cached: serving that cache during an
    /// outage would show last cycle's usage as if it were current. The parse
    /// must reject it, surfacing the live error instead. Mirrors
    /// `cursor::fetch`'s `cache_past_its_billing_reset_is_not_served_during_an_outage`.
    #[tokio::test]
    async fn cache_past_its_quota_reset_is_not_served_during_an_outage() {
        let (_creds_dir, creds_path) = creds_fixture("gho_example");
        let (_cache_dir, cache) = cache_fixture();
        cache
            .write_payload(
                serde_json::to_vec(&serde_json::json!({
                    "entitlement": 300.0, "remaining": 100.0,
                    "unlimited": false, "reset_at": "2026-08-04T00:00:00Z",
                }))
                .unwrap()
                .as_slice(),
            )
            .unwrap();

        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/copilot_internal/user")
            .with_status(503)
            .create_async()
            .await;
        let endpoints = Endpoints {
            user: format!("{}/copilot_internal/user", server.url()),
        };
        let now = DateTime::parse_from_rfc3339("2026-08-05T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let err = fetch_snapshot_at(
            &reqwest::Client::new(),
            &creds_path,
            &cache,
            &endpoints,
            Duration::from_secs(0),
            now,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, AppError::Http { status: 503, .. }));
    }
}
