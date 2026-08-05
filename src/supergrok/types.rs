//! Wire types for SuperGrok CLI billing
//! (`GET …/v1/billing?format=credits`).
//!
//! Confirmed against:
//! - Live SuperGrok OAuth responses (weekly `creditUsagePercent`,
//!   `currentPeriod`, `productUsage`, prepaid cents).
//! - The official Grok Build CLI's own billing extension
//!   (`xai-org/grok-build` `billing.rs`), which documents both the newer
//!   credits-config shape and the legacy monthly-limit shape.
//!
//! Unified-billing accounts often omit percentage fields on the default
//! payload and expose `monthlyLimit`/`used` instead — we fall back to that
//! ratio so `/usage` does not report empty (see oh-my-pi #6388).

use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::error::{AppError, Result};
use crate::usage::{SuperGrokProduct, SuperGrokSnapshot};

/// Top-level response. Live CLI-proxy responses wrap the config; some
/// shapes may also surface tier at the top level.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct BillingResponse {
    pub config: Option<BillingConfig>,
    /// Live responses use camelCase `subscriptionTier`; snake_case kept as alias.
    #[serde(alias = "subscriptionTier", alias = "subscription_tier")]
    pub subscription_tier: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct BillingConfig {
    /// Included credit usage as a percentage of the allowance (0.0–100.0+).
    pub credit_usage_percent: Option<f64>,
    pub current_period: Option<UsagePeriod>,
    /// Deprecated monthly shape (cents).
    pub monthly_limit: Option<Cent>,
    pub used: Option<Cent>,
    pub on_demand_cap: Option<Cent>,
    pub on_demand_used: Option<Cent>,
    pub prepaid_balance: Option<Cent>,
    pub is_unified_billing_user: Option<bool>,
    pub billing_period_start: Option<String>,
    pub billing_period_end: Option<String>,
    pub product_usage: Vec<ProductUsage>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct UsagePeriod {
    #[serde(rename = "type")]
    pub period_type: Option<String>,
    pub start: Option<String>,
    pub end: Option<String>,
}

/// Cent value — live responses use a JSON number; the Management API uses a
/// string. Accept either.
#[derive(Debug, Clone, Deserialize)]
pub struct Cent {
    #[serde(deserialize_with = "de_cent_val")]
    pub val: i64,
}

fn de_cent_val<'de, D>(d: D) -> std::result::Result<i64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = serde_json::Value::deserialize(d)?;
    match v {
        serde_json::Value::Number(n) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f as i64))
            .ok_or_else(|| serde::de::Error::custom("cent val out of range")),
        serde_json::Value::String(s) => s
            .trim()
            .parse::<i64>()
            .map_err(|_| serde::de::Error::custom("cent val string is not an integer")),
        serde_json::Value::Null => Ok(0),
        _ => Err(serde::de::Error::custom("cent val must be a number or string")),
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct ProductUsage {
    pub product: Option<String>,
    pub usage_percent: Option<f64>,
}

pub fn to_snapshot(resp: BillingResponse, account: &str) -> Result<SuperGrokSnapshot> {
    let tier = resp
        .subscription_tier
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or("SuperGrok")
        .to_string();

    let cfg = resp.config.unwrap_or_default();

    let weekly_pct = resolve_usage_percent(&cfg)?;
    let reset_at = resolve_reset_at(&cfg);
    let products = cfg
        .product_usage
        .iter()
        .filter_map(|p| {
            let name = p.product.as_deref()?.trim();
            if name.is_empty() {
                return None;
            }
            let pct = p.usage_percent?;
            if !pct.is_finite() {
                return None;
            }
            Some(SuperGrokProduct {
                name: pretty_product(name),
                pct: clamp_pct(pct),
            })
        })
        .collect();

    let prepaid_balance = cfg.prepaid_balance.map(|c| c.val as f64 / 100.0);

    Ok(SuperGrokSnapshot {
        plan: tier,
        account: account.to_string(),
        weekly_pct,
        reset_at,
        products,
        prepaid_balance,
    })
}

fn resolve_usage_percent(cfg: &BillingConfig) -> Result<i32> {
    if let Some(p) = cfg.credit_usage_percent.filter(|v| v.is_finite()) {
        return Ok(clamp_pct(p));
    }
    // Legacy monthly shape: used/limit in cents.
    let limit = cfg.monthly_limit.as_ref().map(|c| c.val).unwrap_or(0);
    let used = cfg.used.as_ref().map(|c| c.val).unwrap_or(0);
    if limit > 0 {
        let pct = (used as f64 / limit as f64) * 100.0;
        return Ok(clamp_pct(pct));
    }
    // Product rows alone can still give a headline (e.g. only GrokBuild %).
    if let Some(p) = cfg
        .product_usage
        .iter()
        .filter_map(|p| p.usage_percent.filter(|v| v.is_finite()))
        .next()
    {
        return Ok(clamp_pct(p));
    }
    Err(AppError::Schema(
        "supergrok: billing response has no creditUsagePercent, monthly ratio, or productUsage percent"
            .into(),
    ))
}

fn resolve_reset_at(cfg: &BillingConfig) -> Option<DateTime<Utc>> {
    cfg.current_period
        .as_ref()
        .and_then(|p| p.end.as_deref())
        .and_then(parse_dt)
        .or_else(|| cfg.billing_period_end.as_deref().and_then(parse_dt))
}

fn parse_dt(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
        .or_else(|| {
            let normalized = if s.ends_with('Z') {
                format!("{}+00:00", &s[..s.len() - 1])
            } else {
                s.to_string()
            };
            DateTime::parse_from_rfc3339(&normalized)
                .ok()
                .map(|dt| dt.with_timezone(&Utc))
        })
}

fn clamp_pct(p: f64) -> i32 {
    p.round().clamp(0.0, 9999.0) as i32
}

fn pretty_product(name: &str) -> String {
    name.trim()
        .trim_start_matches("PRODUCT_")
        .replace('_', " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credits_shape_parses() {
        let raw = r#"{
          "config": {
            "creditUsagePercent": 2.0,
            "currentPeriod": {
              "type": "USAGE_PERIOD_TYPE_WEEKLY",
              "start": "2026-07-30T01:03:38.030598+00:00",
              "end": "2026-08-06T01:03:38.030598+00:00"
            },
            "productUsage": [
              {"product": "GrokBuild", "usagePercent": 2.0},
              {"product": "GrokChat"}
            ],
            "prepaidBalance": {"val": 0},
            "isUnifiedBillingUser": true
          }
        }"#;
        let resp: BillingResponse = serde_json::from_str(raw).unwrap();
        let snap = to_snapshot(resp, "user-1").unwrap();
        assert_eq!(snap.weekly_pct, 2);
        assert_eq!(snap.plan, "SuperGrok");
        assert_eq!(snap.products.len(), 1);
        assert_eq!(snap.products[0].name, "GrokBuild");
        assert_eq!(snap.products[0].pct, 2);
        assert_eq!(snap.prepaid_balance, Some(0.0));
        assert!(snap.reset_at.is_some());
    }

    #[test]
    fn legacy_monthly_shape_falls_back() {
        let raw = r#"{
          "config": {
            "monthlyLimit": {"val": 2000},
            "used": {"val": 500},
            "billingPeriodEnd": "2026-09-01T00:00:00Z"
          },
          "subscriptionTier": "SuperGrok Heavy"
        }"#;
        let resp: BillingResponse = serde_json::from_str(raw).unwrap();
        let snap = to_snapshot(resp, "u").unwrap();
        assert_eq!(snap.weekly_pct, 25);
        assert_eq!(snap.plan, "SuperGrok Heavy");
        assert!(snap.reset_at.is_some());
    }

    #[test]
    fn string_cents_also_parse() {
        let raw = r#"{"config":{"monthlyLimit":{"val":"1000"},"used":{"val":"250"}}}"#;
        let resp: BillingResponse = serde_json::from_str(raw).unwrap();
        let snap = to_snapshot(resp, "u").unwrap();
        assert_eq!(snap.weekly_pct, 25);
    }

    #[test]
    fn empty_config_is_schema_error() {
        let resp: BillingResponse = serde_json::from_str(r#"{"config":{}}"#).unwrap();
        assert!(to_snapshot(resp, "u").is_err());
    }

    #[test]
    fn product_without_percent_is_skipped() {
        let raw = r#"{
          "config": {
            "creditUsagePercent": 10,
            "productUsage": [{"product": "GrokChat"}, {"product": "Api", "usagePercent": 10}]
          }
        }"#;
        let resp: BillingResponse = serde_json::from_str(raw).unwrap();
        let snap = to_snapshot(resp, "u").unwrap();
        assert_eq!(snap.products.len(), 1);
        assert_eq!(snap.products[0].name, "Api");
    }
}
