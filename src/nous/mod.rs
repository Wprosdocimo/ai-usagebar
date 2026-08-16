//! Nous Research OAuth and subscription usage integration.

pub mod types {
    use serde_json::Value;

    #[derive(Debug)]
    pub struct DeviceCode {
        pub device_code: String,
        pub user_code: String,
        pub verification_uri: String,
        pub expires_in: u64,
        pub interval: u64,
    }

    #[derive(Debug)]
    pub struct TokenResponse {
        pub access_token: String,
        pub refresh_token: String,
        pub expires_in: u64,
    }

    #[derive(Debug)]
    pub struct AccountSnapshot {
        pub plan: Option<String>,
        pub monthly_credits: Option<f64>,
        pub credits_remaining: Option<f64>,
        pub rollover_credits: Option<f64>,
        pub serialized_snapshot: String,
    }

    pub fn parse_device_code(_: &Value) -> Result<DeviceCode, String> {
        Err("Nous device-code parser is not implemented".into())
    }

    pub fn parse_token(_: &Value) -> Result<TokenResponse, String> {
        Err("Nous token parser is not implemented".into())
    }

    pub fn parse_account(_: &Value) -> Result<AccountSnapshot, String> {
        Err("Nous account parser is not implemented".into())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::types::{parse_account, parse_device_code, parse_token};

    fn fixture(path: &str) -> Value {
        serde_json::from_str(path).expect("fixture JSON must be valid")
    }

    #[test]
    fn official_device_fixture_uses_expected_fields() {
        let value = fixture(include_str!("../../tests/fixtures/nous/device-code.json"));
        let parsed = parse_device_code(&value).expect("official device fixture must parse");
        assert_eq!(parsed.device_code, "test-device-code");
        assert_eq!(parsed.user_code, "TEST-USER");
        assert_eq!(
            parsed.verification_uri,
            "https://portal.nousresearch.com/device"
        );
        assert_eq!(parsed.expires_in, 900);
        assert_eq!(parsed.interval, 5);
    }

    #[test]
    fn official_token_fixture_keeps_token_fields_separate() {
        let value = fixture(include_str!("../../tests/fixtures/nous/token-success.json"));
        let parsed = parse_token(&value).expect("official token fixture must parse");
        assert_eq!(parsed.access_token, "test-access-token");
        assert_eq!(parsed.refresh_token, "test-refresh-token");
        assert_eq!(parsed.expires_in, 3600);
    }

    #[test]
    fn official_account_fixture_keeps_internal_identifiers_out_of_snapshot() {
        let value = fixture(include_str!("../../tests/fixtures/nous/account.json"));
        let parsed = parse_account(&value).expect("official account fixture must parse");
        assert_eq!(parsed.plan.as_deref(), Some("Pro"));
        assert_eq!(parsed.monthly_credits, Some(1000.0));
        assert_eq!(parsed.credits_remaining, Some(760.0));
        assert_eq!(parsed.rollover_credits, Some(40.0));
        assert!(!parsed.serialized_snapshot.contains("internal-user"));
        assert!(!parsed.serialized_snapshot.contains("internal-org"));
    }
}
