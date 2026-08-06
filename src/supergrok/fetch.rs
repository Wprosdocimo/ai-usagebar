//! SuperGrok fetch — read OAuth from `~/.grok/auth.json`, refresh when close
//! to expiry (writing rotated tokens back), then
//! `GET …/v1/billing?format=credits` with the Grok CLI host fingerprint.
//!
//! Cache/stale/error-fallback shape mirrors `kiro::fetch` / `cursor::fetch`.

use std::path::Path;
use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::cache::{Cache, MAX_STALE, acquire_lock_async};
use crate::error::{AppError, Result};
use crate::usage::SuperGrokSnapshot;
use crate::vendor::{MAX_BODY_BYTES, read_body_capped};

use super::creds::{self, AccountEntry, AuthFile};
use super::oauth;
use super::types::{self, BillingResponse};

/// CLI chat-proxy billing host. Pinned to HTTPS `*.grok.com` (same host the
/// official Grok Build CLI uses). Not a public documented API.
pub const BILLING_BASE: &str = "https://cli-chat-proxy.grok.com/v1";
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
const LOCK_TIMEOUT: Duration = Duration::from_secs(15);
/// Same gate header the Grok Build CLI sends so subscription billing accepts
/// the OAuth bearer (OMP #4874 / grok-build billing.rs).
const TOKEN_AUTH_HEADER: &str = "xai-grok-cli";

#[derive(Debug, Clone)]
pub struct Endpoints {
    /// Credits-config shape (`?format=credits`) — weekly % when present.
    pub billing: String,
    /// Legacy monthly shape (plain `/billing`) — fill-in for unified accounts
    /// that omit percent fields on the credits endpoint.
    pub billing_legacy: String,
    pub token: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            billing: format!("{BILLING_BASE}/billing?format=credits"),
            billing_legacy: format!("{BILLING_BASE}/billing"),
            token: oauth::TOKEN_URL.to_string(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct FetchOutcome {
    pub snapshot: SuperGrokSnapshot,
    pub stale: bool,
    pub last_error: Option<(u16, String)>,
    pub cache_age: Option<Duration>,
}

pub async fn fetch_snapshot(
    client: &reqwest::Client,
    auth_path: &Path,
    cache: &Cache,
    cache_ttl: Duration,
) -> Result<FetchOutcome> {
    fetch_snapshot_at(
        client,
        auth_path,
        cache,
        cache_ttl,
        &Endpoints::default(),
        Utc::now(),
    )
    .await
}

/// Test seam: inject endpoints + clock.
pub async fn fetch_snapshot_at(
    client: &reqwest::Client,
    auth_path: &Path,
    cache: &Cache,
    cache_ttl: Duration,
    endpoints: &Endpoints,
    now: DateTime<Utc>,
) -> Result<FetchOutcome> {
    cache.ensure_dir()?;
    let _lock = acquire_lock_async(&cache.lock_path(), LOCK_TIMEOUT).await?;

    let mut auth = creds::read_from(auth_path)?;
    let (slot_key, mut entry) = creds::select_account(&auth)?;
    let account = entry.account_key();

    if let Some(bytes) = cache.fresh_payload(cache_ttl)?
        && let Ok(outcome) = reuse_cache(&bytes, cache, false, &account)
        // A snapshot whose period already ended must not keep blocking a
        // refetch (shows "Resets now" + last week's % until TTL expires).
        && !period_has_ended(&outcome.snapshot, now)
    {
        return Ok(outcome);
    }

    // Silent OIDC refresh when close to expiry; always persist rotation.
    if let Err(e) = ensure_fresh_token(
        client, endpoints, auth_path, &mut auth, &slot_key, &mut entry, now,
    )
    .await
    {
        if e.is_transient() {
            return fallback_silent(cache, &account, e);
        }
        // Hard auth failure: still try cache with the error attached.
        cache.mark_stale();
        if let Some((code, msg)) = error_to_pair(&e) {
            cache.write_last_error(code, &msg);
        }
        return fallback_with_error(cache, &account, e);
    }

    match fetch_live(client, endpoints, &entry).await {
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
        Err(e) if e.is_transient() => fallback_silent(cache, &account, e),
        Err(e) => {
            cache.mark_stale();
            if let Some((code, msg)) = error_to_pair(&e) {
                cache.write_last_error(code, &msg);
            }
            fallback_with_error(cache, &account, e)
        }
    }
}

async fn ensure_fresh_token(
    client: &reqwest::Client,
    endpoints: &Endpoints,
    auth_path: &Path,
    auth: &mut AuthFile,
    slot_key: &str,
    entry: &mut AccountEntry,
    now: DateTime<Utc>,
) -> Result<()> {
    if !creds::needs_refresh(entry, now) {
        return Ok(());
    }
    let refresh_token = entry
        .refresh_token
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            AppError::Credentials(
                "SuperGrok: access token expired and no refresh_token is stored. Run `grok login`."
                    .into(),
            )
        })?;
    let client_id = entry.client_id(slot_key)?;
    let resp = oauth::refresh(client, &endpoints.token, &client_id, refresh_token).await?;
    entry.apply_refresh(resp.access_token, resp.refresh_token, resp.expires_in, now);
    auth.accounts.insert(slot_key.to_string(), entry.clone());
    creds::write_back(auth_path, auth)?;
    Ok(())
}

fn period_has_ended(snap: &SuperGrokSnapshot, now: DateTime<Utc>) -> bool {
    snap.reset_at.is_some_and(|r| r <= now)
}

async fn get_billing_json(
    client: &reqwest::Client,
    url: &str,
    entry: &AccountEntry,
) -> Result<BillingResponse> {
    let mut req = client
        .get(url)
        .header("Authorization", format!("Bearer {}", entry.key))
        .header("X-XAI-Token-Auth", TOKEN_AUTH_HEADER)
        .header("User-Agent", "xai-grok-cli")
        .header("Accept", "application/json");
    if let Some(uid) = entry.user_id.as_deref().filter(|s| !s.is_empty()) {
        req = req.header("x-userid", uid);
    }

    let resp = tokio::time::timeout(HTTP_TIMEOUT, req.send())
        .await
        .map_err(|_| AppError::Transport(format!("supergrok billing timeout: {url}")))??;

    let status = resp.status();
    let bytes = read_body_capped(resp, MAX_BODY_BYTES).await?;
    if !status.is_success() {
        let body = String::from_utf8_lossy(&bytes).chars().take(200).collect();
        return Err(AppError::Http {
            status: status.as_u16(),
            body,
        });
    }
    serde_json::from_slice(&bytes).map_err(|e| AppError::Schema(format!("supergrok billing: {e}")))
}

async fn fetch_live(
    client: &reqwest::Client,
    endpoints: &Endpoints,
    entry: &AccountEntry,
) -> Result<SuperGrokSnapshot> {
    let credits = get_billing_json(client, &endpoints.billing, entry).await?;
    // Best-effort merge of the legacy monthly body for unified accounts that
    // omit percent fields on `?format=credits` (oh-my-pi #6388). A failure here
    // must not discard a usable credits response.
    let merged = match get_billing_json(client, &endpoints.billing_legacy, entry).await {
        Ok(legacy) => types::merge_billing(credits, legacy),
        Err(_) => credits,
    };
    types::to_snapshot(merged, &entry.account_key())
}

fn snap_to_json(snap: &SuperGrokSnapshot) -> serde_json::Value {
    serde_json::json!({
        "account": snap.account,
        "snapshot": {
            "plan": snap.plan,
            "account": snap.account,
            "weekly_pct": snap.weekly_pct,
            "reset_at": snap.reset_at.map(|t| t.to_rfc3339()),
            "products": snap.products.iter().map(|p| {
                serde_json::json!({"name": p.name, "pct": p.pct})
            }).collect::<Vec<_>>(),
            "prepaid_balance": snap.prepaid_balance,
        }
    })
}

fn parse_cache(bytes: &[u8], account: &str) -> Result<SuperGrokSnapshot> {
    let v: serde_json::Value = serde_json::from_slice(bytes)?;
    let cached_account = v.get("account").and_then(|x| x.as_str()).unwrap_or("");
    if cached_account != account {
        return Err(AppError::Schema(format!(
            "supergrok cache belongs to a different account ({cached_account}); refetching"
        )));
    }
    let s = v
        .get("snapshot")
        .ok_or_else(|| AppError::Schema("supergrok cache missing 'snapshot'".into()))?;
    let plan = s
        .get("plan")
        .and_then(|x| x.as_str())
        .unwrap_or("SuperGrok")
        .to_string();
    let weekly_pct = s
        .get("weekly_pct")
        .and_then(|x| x.as_i64())
        .ok_or_else(|| AppError::Schema("supergrok cache missing weekly_pct".into()))?
        as i32;
    let reset_at = s
        .get("reset_at")
        .and_then(|x| x.as_str())
        .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
        .map(|t| t.with_timezone(&Utc));
    let products = s
        .get("products")
        .and_then(|x| x.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|p| {
                    Some(crate::usage::SuperGrokProduct {
                        name: p.get("name")?.as_str()?.to_string(),
                        pct: p.get("pct")?.as_i64()? as i32,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let prepaid_balance = s.get("prepaid_balance").and_then(|x| x.as_f64());
    Ok(SuperGrokSnapshot {
        plan,
        account: account.to_string(),
        weekly_pct,
        reset_at,
        products,
        prepaid_balance,
    })
}

fn reuse_cache(bytes: &[u8], cache: &Cache, stale: bool, account: &str) -> Result<FetchOutcome> {
    let snap = parse_cache(bytes, account)?;
    Ok(FetchOutcome {
        snapshot: snap,
        stale,
        last_error: cache.read_last_error(),
        cache_age: cache.payload_age(),
    })
}

fn fallback_silent(cache: &Cache, account: &str, original: AppError) -> Result<FetchOutcome> {
    let Some(bytes) = cache.fallback_payload(MAX_STALE)? else {
        return Err(original);
    };
    match reuse_cache(&bytes, cache, true, account) {
        // Never resurrect a prior period's % after rollover.
        Ok(o) if !period_has_ended(&o.snapshot, Utc::now()) => Ok(o),
        _ => Err(original),
    }
}

fn fallback_with_error(cache: &Cache, account: &str, original: AppError) -> Result<FetchOutcome> {
    let Some(bytes) = cache.fallback_payload(MAX_STALE)? else {
        return Err(original);
    };
    match reuse_cache(&bytes, cache, true, account) {
        Ok(mut o) if !period_has_ended(&o.snapshot, Utc::now()) => {
            o.last_error = error_to_pair(&original);
            Ok(o)
        }
        _ => Err(original),
    }
}

fn error_to_pair(e: &AppError) -> Option<(u16, String)> {
    match e {
        AppError::Http { status, body } => Some((*status, body.clone())),
        other => Some((0, other.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use tempfile::TempDir;

    fn cache_fixture() -> (TempDir, Cache) {
        let td = TempDir::new().unwrap();
        let cache = Cache::at(td.path().join("supergrok"));
        cache.ensure_dir().unwrap();
        (td, cache)
    }

    /// Write auth to a closed path under `dir`. Do **not** keep a file handle
    /// open — `write_back` renames over the target, and Windows denies that
    /// while the original handle is still open.
    fn write_auth(dir: &Path, access: &str, refresh: &str, expires: &str) -> PathBuf {
        let path = dir.join("auth.json");
        let body = format!(
            r#"{{
              "https://auth.x.ai::cid": {{
                "key": "{access}",
                "refresh_token": "{refresh}",
                "expires_at": "{expires}",
                "user_id": "user-1",
                "oidc_client_id": "cid"
              }}
            }}"#
        );
        std::fs::write(&path, body).unwrap();
        path
    }

    #[tokio::test]
    async fn fresh_token_fetches_billing() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/v1/billing")
            .match_query(mockito::Matcher::UrlEncoded(
                "format".into(),
                "credits".into(),
            ))
            .match_header("authorization", "Bearer good-at")
            .match_header("x-xai-token-auth", "xai-grok-cli")
            .with_status(200)
            .with_body(
                r#"{"config":{"creditUsagePercent":12.4,"currentPeriod":{"end":"2026-08-10T00:00:00Z"},"productUsage":[{"product":"GrokBuild","usagePercent":12.4}]}}"#,
            )
            .create_async()
            .await;

        let auth_td = TempDir::new().unwrap();
        let auth = write_auth(
            auth_td.path(),
            "good-at",
            "rt",
            // Far future — no refresh.
            "2099-01-01T00:00:00Z",
        );
        let (_td, cache) = cache_fixture();
        let client = reqwest::Client::new();
        let endpoints = Endpoints {
            billing: format!("{}/v1/billing?format=credits", server.url()),
            billing_legacy: format!("{}/v1/billing", server.url()),
            token: format!("{}/oauth2/token", server.url()),
        };
        let out = fetch_snapshot_at(
            &client,
            &auth,
            &cache,
            Duration::from_secs(0),
            &endpoints,
            Utc::now(),
        )
        .await
        .unwrap();
        assert_eq!(out.snapshot.weekly_pct, 12);
        assert_eq!(out.snapshot.products[0].name, "GrokBuild");
        assert!(!out.stale);
    }

    #[tokio::test]
    async fn expired_token_refreshes_then_fetches() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/oauth2/token")
            .with_status(200)
            .with_body(
                r#"{"access_token":"fresh-at","refresh_token":"fresh-rt","expires_in":21600}"#,
            )
            .create_async()
            .await;
        server
            .mock("GET", "/v1/billing")
            .match_query(mockito::Matcher::UrlEncoded(
                "format".into(),
                "credits".into(),
            ))
            .match_header("authorization", "Bearer fresh-at")
            .with_status(200)
            .with_body(r#"{"config":{"creditUsagePercent":5}}"#)
            .create_async()
            .await;

        let auth_td = TempDir::new().unwrap();
        let auth = write_auth(auth_td.path(), "stale-at", "old-rt", "2020-01-01T00:00:00Z");
        let (_td, cache) = cache_fixture();
        let client = reqwest::Client::new();
        let endpoints = Endpoints {
            billing: format!("{}/v1/billing?format=credits", server.url()),
            billing_legacy: format!("{}/v1/billing", server.url()),
            token: format!("{}/oauth2/token", server.url()),
        };
        let out = fetch_snapshot_at(
            &client,
            &auth,
            &cache,
            Duration::from_secs(0),
            &endpoints,
            Utc::now(),
        )
        .await
        .unwrap();
        assert_eq!(out.snapshot.weekly_pct, 5);

        // Rotated tokens must be written back.
        let rewritten = creds::read_from(&auth).unwrap();
        let (_, entry) = creds::select_account(&rewritten).unwrap();
        assert_eq!(entry.key, "fresh-at");
        assert_eq!(entry.refresh_token.as_deref(), Some("fresh-rt"));
    }

    #[tokio::test]
    async fn fresh_cache_makes_no_network_call() {
        let server = mockito::Server::new_async().await;
        let auth_td = TempDir::new().unwrap();
        let auth = write_auth(auth_td.path(), "good-at", "rt", "2099-01-01T00:00:00Z");
        let (_td, cache) = cache_fixture();
        cache
            .write_payload(
                serde_json::json!({
                    "account": "user-1",
                    "snapshot": {
                        "plan": "SuperGrok",
                        "account": "user-1",
                        "weekly_pct": 7,
                        "reset_at": null,
                        "products": [],
                        "prepaid_balance": null
                    }
                })
                .to_string()
                .as_bytes(),
            )
            .unwrap();

        let client = reqwest::Client::new();
        let endpoints = Endpoints {
            billing: format!("{}/v1/billing?format=credits", server.url()),
            billing_legacy: format!("{}/v1/billing", server.url()),
            token: format!("{}/oauth2/token", server.url()),
        };
        let out = fetch_snapshot_at(
            &client,
            &auth,
            &cache,
            Duration::from_secs(3600),
            &endpoints,
            Utc::now(),
        )
        .await
        .unwrap();
        assert_eq!(out.snapshot.weekly_pct, 7);
    }

    #[tokio::test]
    async fn http_error_falls_back_to_cache() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/v1/billing")
            .match_query(mockito::Matcher::UrlEncoded(
                "format".into(),
                "credits".into(),
            ))
            .with_status(500)
            .with_body("nope")
            .create_async()
            .await;

        let auth_td = TempDir::new().unwrap();
        let auth = write_auth(auth_td.path(), "good-at", "rt", "2099-01-01T00:00:00Z");
        let (_td, cache) = cache_fixture();
        cache
            .write_payload(
                serde_json::json!({
                    "account": "user-1",
                    "snapshot": {
                        "plan": "SuperGrok",
                        "account": "user-1",
                        "weekly_pct": 3,
                        "reset_at": null,
                        "products": [],
                        "prepaid_balance": null
                    }
                })
                .to_string()
                .as_bytes(),
            )
            .unwrap();

        let client = reqwest::Client::new();
        let endpoints = Endpoints {
            billing: format!("{}/v1/billing?format=credits", server.url()),
            billing_legacy: format!("{}/v1/billing", server.url()),
            token: format!("{}/oauth2/token", server.url()),
        };
        let out = fetch_snapshot_at(
            &client,
            &auth,
            &cache,
            Duration::from_secs(0),
            &endpoints,
            Utc::now(),
        )
        .await
        .unwrap();
        assert!(out.stale);
        assert_eq!(out.snapshot.weekly_pct, 3);
        assert_eq!(out.last_error.as_ref().map(|(c, _)| *c), Some(500));
    }
}
