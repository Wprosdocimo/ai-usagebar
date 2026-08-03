//! GitHub OAuth Device Authorization Grant — the interactive `ai-usagebar
//! login copilot` flow.
//!
//! `CLIENT_ID` is not ai-usagebar's own — it's a public client id GitHub's
//! own tooling uses for this exact grant. Registering ai-usagebar's own OAuth
//! App would not help: `copilot_internal/*` is gated by which app issued the
//! token, and only a small allow-listed set qualifies — confirmed live by
//! this project's own `gh auth token` (a *different* OAuth App) getting a 403
//! "scraping" response against the same endpoint.
//!
//! **Two bugs stacked on the first live attempt, on an organization-owned
//! Copilot seat**, both fixed here:
//! 1. **No `USER_AGENT`.** reqwest sends none by default, and GitHub's own
//!    UI flagged the authorization attempt as an "anomalous client" —
//!    plausibly enough on its own to explain an authorization that never
//!    resolves, independent of anything else below.
//! 2. The client id used on that same attempt was `Iv1.b507a08c87ecfe98`
//!    (VS Code Copilot Chat's own, used by e.g. `oh-my-posh` and
//!    `stencila`) — swapped for `Ov23li8tweQw6odWQebz` (the id
//!    `opencode`'s Copilot plugin uses; confirmed via its OSS source, and
//!    via that same account already holding a working token obtained
//!    under this id). Whether this swap was actually load-bearing or the
//!    `USER_AGENT` fix alone would have been enough is unverified — both
//!    changed at once — but this id is GitHub's more current one either way.
use std::future::Future;
use std::time::Duration;

use serde::Deserialize;

use crate::error::{AppError, Result};

pub const CLIENT_ID: &str = "Ov23li8tweQw6odWQebz";
const SCOPE: &str = "read:user";
const DEVICE_CODE_URL: &str = "https://github.com/login/device/code";
const ACCESS_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";

/// A request with **no `User-Agent` at all** (reqwest sends none by default)
/// is itself an anomaly signal to GitHub's abuse heuristics — confirmed live:
/// the switch to this client id alone wasn't enough, GitHub flagged the
/// authorization attempt as an "anomalous client" until this header was
/// added. `opencode`'s Copilot plugin sends `opencode/<version>` on the same
/// two requests; this is the same idea.
const USER_AGENT: &str = concat!("ai-usagebar/", env!("CARGO_PKG_VERSION"));

#[derive(Debug, Clone, Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    expires_in: u64,
    #[serde(default = "default_interval")]
    interval: u64,
}

fn default_interval() -> u64 {
    5
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum PollResponse {
    Ok { access_token: String },
    Pending { error: String },
}

async fn request_device_code(client: &reqwest::Client) -> Result<DeviceCodeResponse> {
    request_device_code_at(client, DEVICE_CODE_URL).await
}

/// Request a device code + user code from GitHub. `url` is a test seam —
/// production always calls the wrapper above with [`DEVICE_CODE_URL`].
async fn request_device_code_at(client: &reqwest::Client, url: &str) -> Result<DeviceCodeResponse> {
    let resp = client
        .post(url)
        .header("Accept", "application/json")
        .header("User-Agent", USER_AGENT)
        .json(&serde_json::json!({ "client_id": CLIENT_ID, "scope": SCOPE }))
        .send()
        .await?;
    let status = resp.status();
    let body = crate::vendor::read_body_capped(resp, crate::vendor::MAX_BODY_BYTES).await?;
    if !status.is_success() {
        return Err(AppError::Http {
            status: status.as_u16(),
            body: "GitHub device-code request failed".into(),
        });
    }
    serde_json::from_slice(&body)
        .map_err(|e| AppError::Schema(format!("github device-code response: {e}")))
}

/// Split out from the loop so the two timeout messages are independently
/// testable without racing the wall clock.
fn timeout_message(last_seen_slow_down: bool) -> &'static str {
    if last_seen_slow_down {
        "GitHub is rate-limiting this login attempt (repeated `slow_down` responses \
         through the whole code lifetime). This is unrelated to authorizing in the \
         browser — wait a while (an hour or more if you've retried several times \
         recently) before running `ai-usagebar login copilot` again."
    } else {
        "GitHub device-code login timed out. Run `ai-usagebar login copilot` again."
    }
}

async fn poll_for_access_token<F, Fut>(
    client: &reqwest::Client,
    device: &DeviceCodeResponse,
    sleep: F,
) -> Result<String>
where
    F: Fn(Duration) -> Fut,
    Fut: Future<Output = ()>,
{
    poll_for_access_token_at(client, ACCESS_TOKEN_URL, device, sleep).await
}

/// Poll GitHub until the user finishes authorizing in the browser, or the
/// device code expires. `url` and `sleep` are test seams — production always
/// calls the wrapper above with [`ACCESS_TOKEN_URL`] and `tokio::time::sleep`.
async fn poll_for_access_token_at<F, Fut>(
    client: &reqwest::Client,
    url: &str,
    device: &DeviceCodeResponse,
    sleep: F,
) -> Result<String>
where
    F: Fn(Duration) -> Fut,
    Fut: Future<Output = ()>,
{
    let deadline = std::time::Instant::now() + Duration::from_secs(device.expires_in);
    let mut interval = Duration::from_secs(device.interval.max(1));
    let mut attempt: u32 = 0;
    // Confirmed live: a burst of prior polling (e.g. repeated login attempts
    // in quick succession) can put GitHub into a state where *every* poll
    // gets `slow_down`, all the way to the 15-minute device-code expiry,
    // without ever surfacing the real authorization state again — even
    // though this loop correctly grows `interval` exactly as each response
    // asks. There is no way to distinguish that from a slow user in-band; this
    // flag only sharpens the *timeout* message when it was the last thing seen.
    let mut last_seen_slow_down = false;

    loop {
        if std::time::Instant::now() >= deadline {
            return Err(AppError::Credentials(
                timeout_message(last_seen_slow_down).into(),
            ));
        }
        sleep(interval).await;
        attempt += 1;
        // Visible heartbeat every ~30s so a long wait reads as "still
        // polling", not "frozen" — whether that's a slow browser step or
        // the rate-limit case above.
        let heartbeat_every = (30 / interval.as_secs().max(1)).max(1) as u32;
        if attempt.is_multiple_of(heartbeat_every) {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            println!(
                "  …still waiting ({}m{}s left before the code expires)",
                remaining.as_secs() / 60,
                remaining.as_secs() % 60
            );
        }

        let resp = client
            .post(url)
            .header("Accept", "application/json")
            .header("User-Agent", USER_AGENT)
            .json(&serde_json::json!({
                "client_id": CLIENT_ID,
                "device_code": device.device_code,
                "grant_type": "urn:ietf:params:oauth:grant-type:device_code",
            }))
            .send()
            .await?;
        let status = resp.status();
        let body = crate::vendor::read_body_capped(resp, crate::vendor::MAX_BODY_BYTES).await?;
        if !status.is_success() {
            return Err(AppError::Http {
                status: status.as_u16(),
                body: "GitHub token poll failed".into(),
            });
        }
        let parsed: PollResponse = serde_json::from_slice(&body)
            .map_err(|e| AppError::Schema(format!("github token-poll response: {e}")))?;

        match parsed {
            PollResponse::Ok { access_token } if !access_token.trim().is_empty() => {
                return Ok(access_token);
            }
            PollResponse::Ok { .. } => {
                return Err(AppError::Schema(
                    "github token-poll response: empty access_token".into(),
                ));
            }
            PollResponse::Pending { error } => match error.as_str() {
                "authorization_pending" => {
                    last_seen_slow_down = false;
                    continue;
                }
                "slow_down" => {
                    last_seen_slow_down = true;
                    // RFC 8628 §3.5: on `slow_down` the client MUST add 5s to
                    // the poll interval (on top of any server-sent `interval`).
                    interval += Duration::from_secs(5);
                    continue;
                }
                "expired_token" => {
                    return Err(AppError::Credentials(
                        "GitHub device code expired. Run `ai-usagebar login copilot` again.".into(),
                    ));
                }
                "access_denied" => {
                    return Err(AppError::Credentials(
                        "GitHub authorization was denied.".into(),
                    ));
                }
                other => {
                    return Err(AppError::Credentials(format!(
                        "GitHub device-code login failed: {other}"
                    )));
                }
            },
        }
    }
}

/// Run the full interactive login: request a device code, print instructions,
/// poll until the user completes it in the browser, and return the GitHub
/// access token. Printing to stdout is deliberate — this is a terminal login
/// command, the same UX category as `claude login` / `codex login`.
pub async fn login(client: &reqwest::Client) -> Result<String> {
    let device = request_device_code(client).await?;
    println!();
    println!("  1. Open: {}", device.verification_uri);
    println!("  2. Enter code: {}", device.user_code);
    println!();
    println!("Waiting for authorization in the browser (Ctrl-C to cancel)…");
    let token = poll_for_access_token(client, &device, |d| tokio::time::sleep(d)).await?;
    println!("Signed in to GitHub Copilot.");
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_sleep() -> impl Fn(Duration) -> std::future::Ready<()> {
        |_d: Duration| std::future::ready(())
    }

    #[tokio::test]
    async fn request_device_code_parses_the_response() {
        let mut server = mockito::Server::new_async().await;
        let m = server
            .mock("POST", "/login/device/code")
            .with_status(200)
            .with_body(
                r#"{"device_code":"dc","user_code":"ABCD-1234",
                    "verification_uri":"https://github.com/login/device",
                    "expires_in":900,"interval":1}"#,
            )
            .create_async()
            .await;
        let client = reqwest::Client::new();
        let resp = request_device_code_at(&client, &format!("{}/login/device/code", server.url()))
            .await
            .unwrap();
        assert_eq!(resp.user_code, "ABCD-1234");
        assert_eq!(resp.interval, 1);
        m.assert_async().await;
    }

    #[tokio::test]
    async fn poll_retries_on_authorization_pending_then_succeeds() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/login/oauth/access_token")
            .with_status(200)
            .with_body(r#"{"error":"authorization_pending"}"#)
            .expect(2)
            .create_async()
            .await;
        server
            .mock("POST", "/login/oauth/access_token")
            .with_status(200)
            .with_body(r#"{"access_token":"gho_final"}"#)
            .create_async()
            .await;

        let device = DeviceCodeResponse {
            device_code: "dc".into(),
            user_code: "ABCD-1234".into(),
            verification_uri: "https://github.com/login/device".into(),
            expires_in: 60,
            interval: 0,
        };
        let client = reqwest::Client::new();
        let token = poll_for_access_token_at(
            &client,
            &format!("{}/login/oauth/access_token", server.url()),
            &device,
            no_sleep(),
        )
        .await
        .unwrap();
        assert_eq!(token, "gho_final");
    }

    #[tokio::test]
    async fn poll_stops_on_access_denied() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/login/oauth/access_token")
            .with_status(200)
            .with_body(r#"{"error":"access_denied"}"#)
            .create_async()
            .await;
        let device = DeviceCodeResponse {
            device_code: "dc".into(),
            user_code: "ABCD-1234".into(),
            verification_uri: "https://github.com/login/device".into(),
            expires_in: 60,
            interval: 0,
        };
        let client = reqwest::Client::new();
        let err = poll_for_access_token_at(
            &client,
            &format!("{}/login/oauth/access_token", server.url()),
            &device,
            no_sleep(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, AppError::Credentials(_)));
    }

    #[tokio::test]
    async fn poll_times_out_when_the_deadline_has_already_passed() {
        let device = DeviceCodeResponse {
            device_code: "dc".into(),
            user_code: "ABCD-1234".into(),
            verification_uri: "https://github.com/login/device".into(),
            expires_in: 0,
            interval: 0,
        };
        let client = reqwest::Client::new();
        // No mock server needed — the deadline check happens before any request.
        let err = poll_for_access_token_at(&client, "http://127.0.0.1:1", &device, no_sleep())
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Credentials(_)));
    }

    #[test]
    fn timeout_message_distinguishes_rate_limiting_from_a_plain_timeout() {
        assert!(timeout_message(false).contains("Run `ai-usagebar login copilot` again"));
        assert!(
            !timeout_message(false)
                .to_lowercase()
                .contains("rate-limiting")
        );
        assert!(
            timeout_message(true)
                .to_lowercase()
                .contains("rate-limiting")
        );
    }

    /// Confirmed live: GitHub can answer every single poll with `slow_down`
    /// for the entire lifetime of a device code, never once returning to
    /// `authorization_pending` or `access_token` — this reproduces that
    /// shape and checks the timeout message reflects it.
    #[tokio::test]
    async fn sustained_slow_down_times_out_with_the_rate_limit_message() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/login/oauth/access_token")
            .with_status(200)
            .with_body(r#"{"error":"slow_down","interval":1}"#)
            .create_async()
            .await;
        let device = DeviceCodeResponse {
            device_code: "dc".into(),
            user_code: "ABCD-1234".into(),
            verification_uri: "https://github.com/login/device".into(),
            expires_in: 1,
            interval: 0,
        };
        let client = reqwest::Client::new();
        let err = poll_for_access_token_at(
            &client,
            &format!("{}/login/oauth/access_token", server.url()),
            &device,
            no_sleep(),
        )
        .await
        .unwrap_err();
        match err {
            AppError::Credentials(m) => assert!(m.to_lowercase().contains("rate-limiting")),
            other => panic!("expected Credentials error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn empty_access_token_is_schema_drift() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/login/oauth/access_token")
            .with_status(200)
            .with_body(r#"{"access_token":""}"#)
            .create_async()
            .await;
        let device = DeviceCodeResponse {
            device_code: "dc".into(),
            user_code: "ABCD-1234".into(),
            verification_uri: "https://github.com/login/device".into(),
            expires_in: 60,
            interval: 0,
        };
        let client = reqwest::Client::new();
        let err = poll_for_access_token_at(
            &client,
            &format!("{}/login/oauth/access_token", server.url()),
            &device,
            no_sleep(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, AppError::Schema(_)));
    }
}
