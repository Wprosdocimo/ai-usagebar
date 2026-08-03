//! Wire schema for `GET api.github.com/copilot_internal/user`.
//!
//! **Reverse-engineered, not documented** — this is the same call
//! `github_copilot_premium_quota_monitor` and `vscode-copilot-insights`
//! (marketplace extensions) and the `Copilot` segment in
//! [`oh-my-posh`](https://github.com/JanDeDobbeleer/oh-my-posh/blob/main/src/segments/copilot.go)
//! make to draw a quota status bar — this vendor's shape matches oh-my-posh's
//! `copilotAPIResponse` field-for-field. Unlike `copilot_internal/v2/token`
//! (which mints a short-lived inference-proxy token this vendor never needs),
//! `/user` accepts the long-lived GitHub OAuth token directly.
//!
//! ```json
//! {
//!   "quota_reset_date": "2026-08-01",
//!   "quota_snapshots": {
//!     "premium_interactions": { "entitlement": 300, "remaining": 287.4, "unlimited": false }
//!   }
//! }
//! ```

use chrono::{DateTime, NaiveDate, Utc};
use serde::Deserialize;

use crate::error::{AppError, Result};
use crate::usage::CopilotSnapshot;

#[derive(Debug, Clone, Deserialize)]
pub struct UserResponse {
    #[serde(default)]
    pub quota_reset_date: Option<String>,
    #[serde(default)]
    pub quota_snapshots: Option<QuotaSnapshots>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct QuotaSnapshots {
    pub premium_interactions: QuotaSnapshot,
}

#[derive(Debug, Clone, Deserialize)]
pub struct QuotaSnapshot {
    #[serde(default)]
    pub entitlement: f64,
    #[serde(default)]
    pub remaining: f64,
    #[serde(default)]
    pub unlimited: bool,
}

pub fn to_snapshot(resp: UserResponse) -> Result<CopilotSnapshot> {
    let snap = resp
        .quota_snapshots
        .ok_or_else(|| AppError::Schema("copilot: missing `quota_snapshots`".into()))?
        .premium_interactions;

    if !snap.entitlement.is_finite() || !snap.remaining.is_finite() {
        return Err(AppError::Schema(
            "copilot: `premium_interactions` entitlement/remaining is not finite".into(),
        ));
    }

    let reset_at = resp
        .quota_reset_date
        .as_deref()
        .map(parse_reset_date)
        .transpose()?;

    Ok(CopilotSnapshot {
        entitlement: snap.entitlement.max(0.0),
        remaining: snap.remaining.max(0.0),
        unlimited: snap.unlimited,
        reset_at,
    })
}

/// GitHub has been observed sending this field as both a bare date
/// (`"2026-08-01"`) and full RFC3339; accept either rather than assuming one.
fn parse_reset_date(s: &str) -> Result<DateTime<Utc>> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Ok(dt.with_timezone(&Utc));
    }
    NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|dt| dt.and_utc())
        .ok_or_else(|| AppError::Schema(format!("copilot: unreadable `quota_reset_date` {s:?}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> UserResponse {
        serde_json::from_str(
            r#"{
                "quota_reset_date": "2026-08-01",
                "quota_snapshots": {
                    "premium_interactions": { "entitlement": 300, "remaining": 287.4, "unlimited": false }
                }
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn parses_the_verified_live_shape() {
        let snap = to_snapshot(sample()).unwrap();
        assert_eq!(snap.entitlement, 300.0);
        assert_eq!(snap.remaining, 287.4);
        assert!(!snap.unlimited);
        assert_eq!(
            snap.reset_at,
            Some(
                NaiveDate::from_ymd_opt(2026, 8, 1)
                    .unwrap()
                    .and_hms_opt(0, 0, 0)
                    .unwrap()
                    .and_utc()
            )
        );
    }

    #[test]
    fn accepts_rfc3339_reset_date_too() {
        let mut resp = sample();
        resp.quota_reset_date = Some("2026-08-01T00:00:00Z".into());
        let snap = to_snapshot(resp).unwrap();
        assert!(snap.reset_at.is_some());
    }

    #[test]
    fn missing_reset_date_is_none_not_an_error() {
        let mut resp = sample();
        resp.quota_reset_date = None;
        let snap = to_snapshot(resp).unwrap();
        assert_eq!(snap.reset_at, None);
    }

    #[test]
    fn unparseable_reset_date_is_schema_drift() {
        let mut resp = sample();
        resp.quota_reset_date = Some("not-a-date".into());
        assert!(matches!(to_snapshot(resp), Err(AppError::Schema(_))));
    }

    #[test]
    fn missing_quota_snapshots_is_schema_drift() {
        let mut resp = sample();
        resp.quota_snapshots = None;
        assert!(matches!(to_snapshot(resp), Err(AppError::Schema(_))));
    }

    #[test]
    fn unlimited_plan_is_parsed_as_is() {
        let resp: UserResponse = serde_json::from_str(
            r#"{
                "quota_snapshots": {
                    "premium_interactions": { "entitlement": 0, "remaining": 0, "unlimited": true }
                }
            }"#,
        )
        .unwrap();
        let snap = to_snapshot(resp).unwrap();
        assert!(snap.unlimited);
    }
}
