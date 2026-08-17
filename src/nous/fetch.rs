//! Nous account transport and response classification.

use std::time::Duration;

use chrono::{DateTime, Utc};
use thiserror::Error;

use super::credentials::CredentialStore;
use super::oauth;
use super::types::{AccountSnapshot, parse_account};

pub const ACCOUNT_URL: &str = "https://portal.nousresearch.com/api/oauth/account";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoints {
    pub account: String,
    pub token: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            account: ACCOUNT_URL.into(),
            token: oauth::TOKEN_URL.into(),
        }
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum FetchError {
    #[error("Nous authentication failed")]
    Authentication,
    #[error("Nous account endpoint rate limited the request")]
    RateLimited,
    #[error("Nous account endpoint is temporarily unavailable")]
    Transient,
    #[error("Nous account response schema mismatch")]
    Schema,
    #[error("Nous account HTTP status {0}")]
    HttpStatus(u16),
    #[error("Nous account network transport failed")]
    Transport,
    #[error("Nous account response exceeded the body limit")]
    BodyLimit,
}

#[derive(Debug, Clone)]
pub struct FetchOutcome {
    pub snapshot: AccountSnapshot,
    pub stale: bool,
    pub last_error: Option<(u16, String)>,
    pub cache_age: Option<Duration>,
}

pub async fn fetch_account(
    client: &reqwest::Client,
    access_token: &str,
    endpoints: &Endpoints,
) -> Result<AccountSnapshot, FetchError> {
    if access_token.trim().is_empty() {
        return Err(FetchError::Authentication);
    }
    let response = client
        .get(&endpoints.account)
        .bearer_auth(access_token)
        .header("accept", "application/json")
        .send()
        .await
        .map_err(|_| FetchError::Transport)?;
    let status = response.status();
    let body = crate::vendor::read_body_capped(response, crate::vendor::MAX_BODY_BYTES)
        .await
        .map_err(|error| {
            if error.to_string().contains("exceeds") {
                FetchError::BodyLimit
            } else {
                FetchError::Transport
            }
        })?;
    if !status.is_success() {
        return Err(classify_status(status.as_u16()));
    }
    let value: serde_json::Value = serde_json::from_slice(&body).map_err(|_| FetchError::Schema)?;
    parse_account(&value).map_err(|_| FetchError::Schema)
}

pub async fn fetch_account_with_refresh(
    client: &reqwest::Client,
    store: &CredentialStore,
    endpoints: &Endpoints,
    now: DateTime<Utc>,
) -> Result<AccountSnapshot, FetchError> {
    let credential = oauth::refresh_if_needed(client, store, &endpoints.token, now)
        .await
        .map_err(map_oauth_error)?;
    fetch_account(client, &credential.access_token, endpoints).await
}

/// Refresh-only seam for callers that want to perform additional local work
/// before the account probe.  Rotation is persisted before this returns.
pub async fn access_credential_for_probe(
    client: &reqwest::Client,
    store: &CredentialStore,
    token_endpoint: &str,
    now: DateTime<Utc>,
) -> Result<super::credentials::NousCredential, FetchError> {
    oauth::refresh_if_needed(client, store, token_endpoint, now)
        .await
        .map_err(map_oauth_error)
}

fn classify_status(status: u16) -> FetchError {
    match status {
        401 | 403 => FetchError::Authentication,
        429 => FetchError::RateLimited,
        500..=599 => FetchError::Transient,
        other => FetchError::HttpStatus(other),
    }
}

fn map_oauth_error(error: oauth::OAuthError) -> FetchError {
    match error {
        oauth::OAuthError::Transport => FetchError::Transport,
        oauth::OAuthError::RefreshTokenRejected
        | oauth::OAuthError::Credentials
        | oauth::OAuthError::AccessDenied
        | oauth::OAuthError::ExpiredToken => FetchError::Authentication,
        oauth::OAuthError::Schema => FetchError::Schema,
        oauth::OAuthError::HttpStatus(429) => FetchError::RateLimited,
        oauth::OAuthError::HttpStatus(status) if status >= 500 => FetchError::Transient,
        oauth::OAuthError::HttpStatus(status) => FetchError::HttpStatus(status),
        oauth::OAuthError::UnknownOAuthError | oauth::OAuthError::Deadline => FetchError::Schema,
    }
}

#[cfg(test)]
mod tests {
    use chrono::{Duration as ChronoDuration, Utc};
    use tempfile::TempDir;

    use super::*;
    use crate::nous::credentials::{CredentialDocument, CredentialStore, NousCredential};

    #[tokio::test]
    async fn account_request_uses_exact_path_bearer_and_accept_headers() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("GET", "/api/oauth/account")
            .match_header("authorization", "Bearer test-access-token")
            .match_header("accept", "application/json")
            .with_status(200)
            .with_body(include_str!("../../tests/fixtures/nous/account.json"))
            .create_async()
            .await;
        let endpoints = Endpoints {
            account: format!("{}/api/oauth/account", server.url()),
            token: format!("{}/token", server.url()),
        };

        let snapshot = fetch_account(&reqwest::Client::new(), "test-access-token", &endpoints)
            .await
            .unwrap();
        assert_eq!(snapshot.plan.as_deref(), Some("Pro"));
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn account_statuses_are_classified_without_retaining_response_bodies() {
        for (status, expected) in [
            (401, FetchError::Authentication),
            (403, FetchError::Authentication),
            (429, FetchError::RateLimited),
            (500, FetchError::Transient),
        ] {
            let mut server = mockito::Server::new_async().await;
            server
                .mock("GET", "/account")
                .with_status(status)
                .with_body("test-secret-response-body")
                .create_async()
                .await;
            let endpoints = Endpoints {
                account: format!("{}/account", server.url()),
                token: format!("{}/token", server.url()),
            };
            let error = fetch_account(&reqwest::Client::new(), "test-access-token", &endpoints)
                .await
                .unwrap_err();
            assert_eq!(error, expected);
            assert!(!format!("{error:?}").contains("test-secret-response-body"));
        }
    }

    #[tokio::test]
    async fn malformed_success_is_a_schema_error_and_network_failure_is_transient() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/account")
            .with_status(200)
            .with_body(r#"{"error":"test-secret"}"#)
            .create_async()
            .await;
        let endpoints = Endpoints {
            account: format!("{}/account", server.url()),
            token: format!("{}/token", server.url()),
        };
        assert_eq!(
            fetch_account(&reqwest::Client::new(), "test-access-token", &endpoints)
                .await
                .unwrap_err(),
            FetchError::Schema
        );

        let network = Endpoints {
            account: "http://127.0.0.1:1/account".into(),
            token: "http://127.0.0.1:1/token".into(),
        };
        assert_eq!(
            fetch_account(&reqwest::Client::new(), "test-access-token", &network)
                .await
                .unwrap_err(),
            FetchError::Transport
        );
    }

    #[tokio::test]
    async fn refresh_is_persisted_before_account_probe() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/token")
            .match_header("x-nous-refresh-token", "test-old-refresh")
            .with_status(200)
            .with_body(r#"{"access_token":"test-new-access","refresh_token":"test-new-refresh","token_type":"Bearer","expires_in":3600}"#)
            .create_async()
            .await;
        let account_mock = server
            .mock("GET", "/account")
            .match_header("authorization", "Bearer test-new-access")
            .with_status(200)
            .with_body(include_str!("../../tests/fixtures/nous/account.json"))
            .create_async()
            .await;
        let root = TempDir::new().unwrap();
        let path = root.path().join("config").join("credentials.json");
        let store = CredentialStore::at(&path);
        store
            .write(&CredentialDocument::new(Some(NousCredential {
                client_id: "hermes-cli".into(),
                access_token: "test-old-access".into(),
                refresh_token: "test-old-refresh".into(),
                expires_at: Utc::now() + ChronoDuration::seconds(100),
            })))
            .unwrap();
        let endpoints = Endpoints {
            account: format!("{}/account", server.url()),
            token: format!("{}/token", server.url()),
        };

        let snapshot =
            fetch_account_with_refresh(&reqwest::Client::new(), &store, &endpoints, Utc::now())
                .await
                .unwrap();
        assert_eq!(snapshot.plan.as_deref(), Some("Pro"));
        assert_eq!(
            store.read().unwrap().unwrap().nous.unwrap().refresh_token,
            "test-new-refresh"
        );
        account_mock.assert_async().await;
    }
}
