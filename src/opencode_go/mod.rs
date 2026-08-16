//! OpenCode Go subscription quota integration.

pub mod types {
    use chrono::{DateTime, Utc};
    use serde_json::Value;

    #[derive(Debug)]
    pub struct Window {
        pub percent: f64,
        pub resets_at: DateTime<Utc>,
    }

    #[derive(Debug)]
    pub struct Usage {
        pub rolling: Option<Window>,
        pub weekly: Option<Window>,
        pub monthly: Option<Window>,
    }

    pub fn parse_usage(_: &Value) -> Result<Usage, String> {
        Err("OpenCode Go usage parser is not implemented".into())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::types::parse_usage;

    #[test]
    fn official_usage_fixture_uses_percent_not_usage_percent() {
        let value: Value =
            serde_json::from_str(include_str!("../../tests/fixtures/opencode-go/usage.json"))
                .expect("fixture JSON must be valid");
        let parsed = parse_usage(&value).expect("official usage fixture must parse");
        assert_eq!(parsed.rolling.expect("rolling").percent, 12.3);
        assert_eq!(parsed.weekly.expect("weekly").percent, 45.6);
        assert_eq!(parsed.monthly.expect("monthly").percent, 78.9);
    }

    #[test]
    fn obsolete_usage_percent_field_is_not_accepted() {
        let value: Value = serde_json::json!({
            "usage": {
                "rolling": {
                    "status": "ok",
                    "usagePercent": 12.3,
                    "resetsAt": "2026-08-16T20:00:00Z"
                }
            }
        });
        assert!(parse_usage(&value).is_err());
    }
}
