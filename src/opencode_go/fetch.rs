use std::time::Duration;

use crate::cache::{Cache, MAX_STALE, acquire_lock_async};
use crate::error::{AUTH_FAILURE_MESSAGE, AppError, Result};
use crate::vendor::{MAX_BODY_BYTES, read_body_capped};

use super::types::{Usage, parse_usage};

pub const BASE_URL: &str = "https://opencode.ai/zen/go/v1/usage";

const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
const LOCK_TIMEOUT: Duration = Duration::from_secs(15);
const SCHEMA_ERROR: &str = "OpenCode Go usage response schema mismatch";

#[derive(Debug, Clone)]
pub struct Endpoints {
    pub usage: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            usage: BASE_URL.to_string(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct FetchOutcome {
    pub snapshot: Usage,
    pub stale: bool,
    pub last_error: Option<(u16, String)>,
    pub cache_age: Option<Duration>,
}

pub async fn fetch_snapshot(
    client: &reqwest::Client,
    api_key: &str,
    cache: &Cache,
    endpoints: &Endpoints,
    ttl: Duration,
) -> Result<FetchOutcome> {
    cache.ensure_dir()?;
    let _lock = acquire_lock_async(&cache.lock_path(), LOCK_TIMEOUT).await?;

    if let Some(bytes) = cache.fresh_payload(ttl)?
        && let Ok(snapshot) = parse_payload(&bytes)
    {
        return Ok(FetchOutcome {
            snapshot,
            stale: false,
            last_error: cache.read_last_error(),
            cache_age: cache.payload_age(),
        });
    }

    match fetch_live(client, &endpoints.usage, api_key).await {
        Ok((body, snapshot)) => {
            cache.write_payload(&body)?;
            Ok(FetchOutcome {
                snapshot,
                stale: false,
                last_error: None,
                cache_age: Some(Duration::ZERO),
            })
        }
        Err(error @ AppError::Transport(_)) => fallback_or_error(cache, None, error),
        Err(AppError::Http { status, .. }) => {
            let message = status_message(status).to_string();
            cache.mark_stale();
            cache.write_last_error(status, &message);
            fallback_or_error(
                cache,
                Some((status, message.clone())),
                AppError::Http {
                    status,
                    body: message,
                },
            )
        }
        Err(AppError::Schema(_)) => {
            let message = SCHEMA_ERROR.to_string();
            cache.mark_stale();
            cache.write_last_error(0, &message);
            fallback_or_error(cache, Some((0, message.clone())), AppError::Schema(message))
        }
        Err(error) => fallback_or_error(cache, None, error),
    }
}

async fn fetch_live(
    client: &reqwest::Client,
    url: &str,
    api_key: &str,
) -> Result<(Vec<u8>, Usage)> {
    let response = tokio::time::timeout(
        HTTP_TIMEOUT,
        client
            .get(url)
            .bearer_auth(api_key)
            .header(reqwest::header::ACCEPT, "application/json")
            .send(),
    )
    .await
    .map_err(|_| AppError::Transport("OpenCode Go request timed out".to_string()))??;

    let status = response.status();
    let body = read_body_capped(response, MAX_BODY_BYTES).await?;
    if !status.is_success() {
        return Err(AppError::Http {
            status: status.as_u16(),
            body: status_message(status.as_u16()).to_string(),
        });
    }

    let snapshot = parse_payload(&body)?;
    Ok((body, snapshot))
}

fn parse_payload(body: &[u8]) -> Result<Usage> {
    let value: serde_json::Value =
        serde_json::from_slice(body).map_err(|_| AppError::Schema(SCHEMA_ERROR.to_string()))?;
    parse_usage(&value).map_err(|_| AppError::Schema(SCHEMA_ERROR.to_string()))
}

fn status_message(status: u16) -> &'static str {
    match status {
        401 | 403 => AUTH_FAILURE_MESSAGE,
        429 => "OpenCode Go request was rate limited",
        500..=599 => "OpenCode Go service is temporarily unavailable",
        _ => "OpenCode Go request failed",
    }
}

fn fallback_or_error(
    cache: &Cache,
    last_error: Option<(u16, String)>,
    error: AppError,
) -> Result<FetchOutcome> {
    if let Some(snapshot) = cached_outcome(cache, last_error)? {
        return Ok(snapshot);
    }
    Err(error)
}

fn cached_outcome(
    cache: &Cache,
    last_error: Option<(u16, String)>,
) -> Result<Option<FetchOutcome>> {
    let Some(body) = cache.fallback_payload(MAX_STALE)? else {
        return Ok(None);
    };
    let Ok(snapshot) = parse_payload(&body) else {
        return Ok(None);
    };
    Ok(Some(FetchOutcome {
        snapshot,
        stale: true,
        last_error: last_error.or_else(|| cache.read_last_error()),
        cache_age: cache.payload_age(),
    }))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tempfile::TempDir;

    use super::*;
    use crate::cache::Cache;

    const GOOD_BODY: &str = r#"{
        "usage": {
            "rolling": {"status":"ok","percent":12.3,"resetsAt":"2026-08-16T20:00:00Z"},
            "weekly": {"status":"ok","percent":45.6,"resetsAt":"2026-08-20T00:00:00Z"},
            "monthly": {"status":"ok","percent":78.9,"resetsAt":"2026-09-01T00:00:00Z"}
        }
    }"#;

    fn cache_fixture() -> (TempDir, Cache) {
        let dir = TempDir::new().expect("temporary cache directory");
        let cache = Cache::at(dir.path().join("opencode-go"));
        cache.ensure_dir().expect("cache directory");
        (dir, cache)
    }

    #[tokio::test]
    async fn fetches_usage_with_bearer_and_accept_headers() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("GET", "/zen/go/v1/usage")
            .match_header("authorization", "Bearer test-key")
            .match_header("accept", "application/json")
            .with_status(200)
            .with_body(GOOD_BODY)
            .create_async()
            .await;
        let (_dir, cache) = cache_fixture();
        let endpoints = Endpoints {
            usage: format!("{}/zen/go/v1/usage", server.url()),
        };

        let output = fetch_snapshot(
            &reqwest::Client::new(),
            "test-key",
            &cache,
            &endpoints,
            Duration::from_secs(60),
        )
        .await
        .expect("successful usage response");

        assert_eq!(output.snapshot.rolling.expect("rolling").percent, 12.3);
        assert!(!output.stale);
        assert!(output.last_error.is_none());
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn unauthorized_without_cache_returns_redacted_error() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/usage")
            .with_status(401)
            .with_body("secret-token should never be returned")
            .create_async()
            .await;
        let (_dir, cache) = cache_fixture();
        let endpoints = Endpoints {
            usage: format!("{}/usage", server.url()),
        };

        let error = fetch_snapshot(
            &reqwest::Client::new(),
            "test-key",
            &cache,
            &endpoints,
            Duration::ZERO,
        )
        .await
        .expect_err("401 without a cache must fail");

        let rendered = error.to_string();
        assert!(rendered.contains("401"));
        assert!(!rendered.contains("secret-token"));
        assert!(!rendered.contains("test-key"));
    }

    #[tokio::test]
    async fn schema_error_does_not_include_response_body() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/usage")
            .with_status(200)
            .with_body(
                r#"{"error":"schema-secret", "usage":{"rolling":{"status":"ok","percent":"schema-secret","resetsAt":"2026-08-16T20:00:00Z"}}}"#,
            )
            .create_async()
            .await;
        let (_dir, cache) = cache_fixture();
        let endpoints = Endpoints {
            usage: format!("{}/usage", server.url()),
        };

        let error = fetch_snapshot(
            &reqwest::Client::new(),
            "test-key",
            &cache,
            &endpoints,
            Duration::ZERO,
        )
        .await
        .expect_err("schema mismatch must fail");

        let rendered = error.to_string();
        assert!(rendered.to_ascii_lowercase().contains("schema"));
        assert!(!rendered.contains("schema-secret"));
        assert!(!rendered.contains("test-key"));
    }

    #[tokio::test]
    async fn status_errors_are_redacted_for_403_429_and_server_errors() {
        for status in [403, 429, 500, 503] {
            let mut server = mockito::Server::new_async().await;
            server
                .mock("GET", "/usage")
                .with_status(status)
                .with_body("body-secret")
                .create_async()
                .await;
            let (_dir, cache) = cache_fixture();
            let endpoints = Endpoints {
                usage: format!("{}/usage", server.url()),
            };

            let error = fetch_snapshot(
                &reqwest::Client::new(),
                "test-key",
                &cache,
                &endpoints,
                Duration::ZERO,
            )
            .await
            .expect_err("status without cache must fail");
            let rendered = error.to_string();
            assert!(rendered.contains(&status.to_string()));
            assert!(!rendered.contains("body-secret"));
            assert!(!rendered.contains("test-key"));
        }
    }
}
