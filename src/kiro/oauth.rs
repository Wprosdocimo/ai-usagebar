//! AWS SSO OIDC token refresh — `POST https://oidc.<region>.amazonaws.com/token`.
//! Public, documented API (`CreateToken`:
//! <https://docs.aws.amazon.com/singlesignon/latest/OIDCAPIReference/API_CreateToken.html>),
//! unlike `fetch.rs`'s reverse-engineered `GetUsageLimits` call.
//!
//! kiro-cli's own cached access token lives about an hour (see `db.rs`); this
//! refreshes it in-memory using the `refresh_token` + `client_id`/`client_secret`
//! kiro-cli already registered for itself at `kiro-cli login` time — nothing
//! new to authenticate, just the same local credentials `db.rs` already read.
//! The refreshed token is **not** written back to kiro-cli's own database
//! (mirroring `db.rs`'s read-only treatment of that live file); it lives only
//! in this process and in ai-usagebar's own snapshot cache.

use serde::{Deserialize, Serialize};

use crate::error::{AppError, Result};

/// Refresh happens this far ahead of the cached expiry so a slow round-trip
/// never races the token's actual death. Mirrors `openai::oauth::REFRESH_BUFFER_SECS`.
pub const REFRESH_BUFFER_SECS: i64 = 300;

pub fn token_endpoint(region: &str) -> String {
    format!("https://oidc.{region}.amazonaws.com/token")
}

#[derive(Debug, Serialize)]
struct RefreshRequest<'a> {
    #[serde(rename = "clientId")]
    client_id: &'a str,
    #[serde(rename = "clientSecret")]
    client_secret: &'a str,
    #[serde(rename = "grantType")]
    grant_type: &'a str,
    #[serde(rename = "refreshToken")]
    refresh_token: &'a str,
}

#[derive(Debug, Deserialize)]
pub struct RefreshResponse {
    #[serde(rename = "accessToken", deserialize_with = "de_nonempty_string")]
    pub access_token: String,
    #[serde(
        rename = "expiresIn",
        default,
        deserialize_with = "de_opt_positive_u64"
    )]
    pub expires_in: Option<u64>,
}

fn de_nonempty_string<'de, D>(d: D) -> std::result::Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = String::deserialize(d)?;
    if value.trim().is_empty() {
        Err(serde::de::Error::custom("accessToken cannot be empty"))
    } else {
        Ok(value)
    }
}

fn de_opt_positive_u64<'de, D>(d: D) -> std::result::Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = serde_json::Value::deserialize(d)?;
    match v {
        serde_json::Value::Null => Ok(None),
        serde_json::Value::Number(n) => {
            const MAX_SAFE: u64 = (i64::MAX as u64) / 2;
            if let Some(value) = n.as_u64().filter(|value| *value <= MAX_SAFE) {
                Ok(Some(value))
            } else {
                Err(serde::de::Error::custom(
                    "expiresIn must be a non-negative integer in range",
                ))
            }
        }
        _ => Err(serde::de::Error::custom(
            "expiresIn must be a number or null",
        )),
    }
}

/// Refresh the access token against `endpoint` (build it with
/// [`token_endpoint`] for production; tests point it at mockito instead).
/// Never echoes the upstream error body verbatim — an `invalid_grant`
/// response from an OAuth token endpoint is not guaranteed not to include
/// account-identifying detail, same reasoning as `cursor::fetch::error_to_pair`.
pub async fn refresh(
    client: &reqwest::Client,
    endpoint: &str,
    client_id: &str,
    client_secret: &str,
    refresh_token: &str,
) -> Result<RefreshResponse> {
    let req = RefreshRequest {
        client_id,
        client_secret,
        grant_type: "refresh_token",
        refresh_token,
    };

    let resp = client
        .post(endpoint)
        .header("Content-Type", "application/json")
        .json(&req)
        .send()
        .await?;

    let status = resp.status();
    let body = crate::vendor::read_body_capped(resp, crate::vendor::MAX_BODY_BYTES).await?;
    if !status.is_success() {
        return Err(AppError::Http {
            status: status.as_u16(),
            body: "Kiro CLI token refresh failed".into(),
        });
    }
    serde_json::from_slice(&body)
        .map_err(|e| AppError::Schema(format!("kiro token refresh response: {e}")))
}

pub fn needs_refresh(expires_at_secs: i64, now_secs: i64) -> bool {
    expires_at_secs < now_secs + REFRESH_BUFFER_SECS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_endpoint_is_region_scoped() {
        assert_eq!(
            token_endpoint("us-east-1"),
            "https://oidc.us-east-1.amazonaws.com/token"
        );
    }

    #[test]
    fn needs_refresh_threshold() {
        let now = 1_000_000;
        assert!(needs_refresh(now + 100, now));
        assert!(!needs_refresh(now + 1000, now));
    }

    #[test]
    fn empty_access_token_is_rejected() {
        let body = r#"{"accessToken":"","expiresIn":3600}"#;
        assert!(serde_json::from_str::<RefreshResponse>(body).is_err());
    }

    #[test]
    fn malformed_expires_in_is_rejected_not_dropped() {
        for value in [r#""3600""#, "-1", "true"] {
            let body = format!(r#"{{"accessToken":"new","expiresIn":{value}}}"#);
            assert!(
                serde_json::from_str::<RefreshResponse>(&body).is_err(),
                "{body}"
            );
        }
        let response: RefreshResponse =
            serde_json::from_str(r#"{"accessToken":"new","expiresIn":null}"#).unwrap();
        assert_eq!(response.expires_in, None);
    }

    #[tokio::test]
    async fn refresh_success_parses_the_new_token() {
        let mut server = mockito::Server::new_async().await;
        let m = server
            .mock("POST", "/token")
            .with_status(200)
            .with_body(r#"{"accessToken":"new-at","tokenType":"Bearer","expiresIn":3600}"#)
            .create_async()
            .await;
        let client = reqwest::Client::new();
        let r = refresh(
            &client,
            &format!("{}/token", server.url()),
            "cid",
            "csecret",
            "old-rt",
        )
        .await
        .unwrap();
        assert_eq!(r.access_token, "new-at");
        assert_eq!(r.expires_in, Some(3600));
        m.assert_async().await;
    }

    #[tokio::test]
    async fn refresh_sends_the_expected_json_body() {
        let mut server = mockito::Server::new_async().await;
        let m = server
            .mock("POST", "/token")
            .match_body(mockito::Matcher::Json(serde_json::json!({
                "clientId": "cid",
                "clientSecret": "csecret",
                "grantType": "refresh_token",
                "refreshToken": "old-rt",
            })))
            .with_status(200)
            .with_body(r#"{"accessToken":"new-at","expiresIn":3600}"#)
            .create_async()
            .await;
        let client = reqwest::Client::new();
        refresh(
            &client,
            &format!("{}/token", server.url()),
            "cid",
            "csecret",
            "old-rt",
        )
        .await
        .unwrap();
        m.assert_async().await;
    }

    #[tokio::test]
    async fn refresh_400_does_not_echo_the_body() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/token")
            .with_status(400)
            .with_body(r#"{"error":"invalid_grant","error_description":"sensitive detail"}"#)
            .create_async()
            .await;
        let client = reqwest::Client::new();
        let err = refresh(
            &client,
            &format!("{}/token", server.url()),
            "cid",
            "csecret",
            "old-rt",
        )
        .await
        .unwrap_err();
        match err {
            AppError::Http { status, body } => {
                assert_eq!(status, 400);
                assert!(!body.contains("sensitive detail"));
            }
            other => panic!("expected Http error, got {other:?}"),
        }
    }
}
