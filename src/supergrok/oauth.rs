//! OIDC token refresh against `https://auth.x.ai/oauth2/token`.
//!
//! Documented OpenID Connect token endpoint (discovered via
//! `https://auth.x.ai/.well-known/openid-configuration`). The Grok Build CLI
//! stores a `refresh_token` + `oidc_client_id` in `~/.grok/auth.json`; we reuse
//! both as a public client (no client secret) — the same grant the CLI itself
//! uses for silent refresh.
//!
//! Upstream error bodies are **not** echoed: OAuth token endpoints may include
//! account-identifying detail in `error_description` (same reasoning as
//! `kiro::oauth` and `cursor::fetch::error_to_pair`).

use serde::{Deserialize, Serialize};

use crate::error::{AppError, Result};

pub const TOKEN_URL: &str = "https://auth.x.ai/oauth2/token";
/// Refresh this far ahead of expiry so a slow round-trip never races the
/// token's actual death. Mirrors `openai::oauth::REFRESH_BUFFER_SECS`.
pub const REFRESH_BUFFER_SECS: i64 = 300;

#[derive(Debug, Serialize)]
struct RefreshRequest<'a> {
    grant_type: &'a str,
    refresh_token: &'a str,
    client_id: &'a str,
}

#[derive(Debug, Deserialize)]
pub struct RefreshResponse {
    #[serde(deserialize_with = "de_nonempty_string")]
    pub access_token: String,
    #[serde(default, deserialize_with = "de_opt_nonempty_string")]
    pub refresh_token: Option<String>,
    #[serde(default, deserialize_with = "de_expires_in")]
    pub expires_in: Option<u64>,
}

fn de_nonempty_string<'de, D>(d: D) -> std::result::Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = String::deserialize(d)?;
    if value.trim().is_empty() {
        Err(serde::de::Error::custom("access_token cannot be empty"))
    } else {
        Ok(value)
    }
}

fn de_opt_nonempty_string<'de, D>(d: D) -> std::result::Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<String>::deserialize(d)?
        .map(|value| {
            if value.trim().is_empty() {
                Err(serde::de::Error::custom("refresh_token cannot be empty"))
            } else {
                Ok(value)
            }
        })
        .transpose()
}

fn de_expires_in<'de, D>(d: D) -> std::result::Result<Option<u64>, D::Error>
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
            } else if let Some(value) = n.as_f64()
                && value.is_finite()
                && value.fract() == 0.0
                && (0.0..=MAX_SAFE as f64).contains(&value)
            {
                Ok(Some(value as u64))
            } else {
                Err(serde::de::Error::custom(
                    "expires_in must be a non-negative integer in range",
                ))
            }
        }
        _ => Err(serde::de::Error::custom(
            "expires_in must be a number or null",
        )),
    }
}

pub fn needs_refresh(expires_at_secs: i64, now_secs: i64) -> bool {
    expires_at_secs < now_secs + REFRESH_BUFFER_SECS
}

/// Refresh against `endpoint` (production uses [`TOKEN_URL`]; tests point it
/// at mockito).
pub async fn refresh(
    client: &reqwest::Client,
    endpoint: &str,
    client_id: &str,
    refresh_token: &str,
) -> Result<RefreshResponse> {
    let body = RefreshRequest {
        grant_type: "refresh_token",
        refresh_token,
        client_id,
    };

    let resp = client
        .post(endpoint)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .form(&body)
        .send()
        .await?;

    let status = resp.status();
    let bytes = crate::vendor::read_body_capped(resp, crate::vendor::MAX_BODY_BYTES).await?;
    if !status.is_success() {
        return Err(AppError::Http {
            status: status.as_u16(),
            body: "SuperGrok OAuth token refresh failed — run `grok login`".into(),
        });
    }
    serde_json::from_slice(&bytes)
        .map_err(|e| AppError::Schema(format!("supergrok token refresh response: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn needs_refresh_threshold() {
        let now = 1_000_000;
        assert!(needs_refresh(now + 100, now));
        assert!(!needs_refresh(now + 1000, now));
    }

    #[test]
    fn empty_access_token_is_rejected() {
        let body = r#"{"access_token":"","expires_in":3600}"#;
        assert!(serde_json::from_str::<RefreshResponse>(body).is_err());
    }

    #[test]
    fn empty_rotated_refresh_token_is_rejected() {
        let body = r#"{"access_token":"new","refresh_token":" ","expires_in":3600}"#;
        assert!(serde_json::from_str::<RefreshResponse>(body).is_err());
    }

    #[test]
    fn malformed_expires_in_is_rejected() {
        // `null` is intentionally accepted as `None` (caller falls back to a
        // default TTL) — only non-numeric / out-of-range values error.
        for value in [r#""3600""#, "-1", "true", "{}"] {
            let body = format!(r#"{{"access_token":"new","expires_in":{value}}}"#);
            assert!(
                serde_json::from_str::<RefreshResponse>(&body).is_err(),
                "{body}"
            );
        }
        let ok_null = r#"{"access_token":"new","expires_in":null}"#;
        let r = serde_json::from_str::<RefreshResponse>(ok_null).unwrap();
        assert!(r.expires_in.is_none());
    }

    #[tokio::test]
    async fn refresh_success_parses_the_new_token() {
        let mut server = mockito::Server::new_async().await;
        let m = server
            .mock("POST", "/oauth2/token")
            .with_status(200)
            .with_body(r#"{"access_token":"new-at","token_type":"Bearer","expires_in":21600,"refresh_token":"new-rt"}"#)
            .create_async()
            .await;
        let client = reqwest::Client::new();
        let r = refresh(
            &client,
            &format!("{}/oauth2/token", server.url()),
            "client-id",
            "old-rt",
        )
        .await
        .unwrap();
        assert_eq!(r.access_token, "new-at");
        assert_eq!(r.expires_in, Some(21600));
        assert_eq!(r.refresh_token.as_deref(), Some("new-rt"));
        m.assert_async().await;
    }

    #[tokio::test]
    async fn refresh_400_does_not_echo_the_body() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/oauth2/token")
            .with_status(400)
            .with_body(r#"{"error":"invalid_grant","error_description":"sensitive detail"}"#)
            .create_async()
            .await;
        let client = reqwest::Client::new();
        let err = refresh(
            &client,
            &format!("{}/oauth2/token", server.url()),
            "client-id",
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
