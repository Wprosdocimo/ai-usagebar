//! Source-compatibility checks for renderer helpers used by library consumers.

use ai_usagebar::minimax::vendor as minimax_vendor;
use ai_usagebar::usage::{MinimaxSnapshot, UsageWindow, ZaiSnapshot};
use ai_usagebar::zai::vendor as zai_vendor;
use chrono::{TimeZone, Utc};

#[test]
fn pace_placeholder_builders_keep_their_two_argument_api() {
    let now = Utc.with_ymd_and_hms(2026, 8, 19, 12, 0, 0).unwrap();
    let window = UsageWindow {
        utilization_pct: 25,
        resets_at: Some(now + chrono::Duration::hours(1)),
        window_duration: chrono::Duration::hours(5),
    };

    let zai = ZaiSnapshot {
        plan: "GLM Coding Pro".into(),
        session: Some(window.clone()),
        weekly: None,
        mcp: None,
    };
    assert_eq!(
        zai_vendor::build_placeholders(&zai, now)["session_pct"],
        "25"
    );

    let minimax = MinimaxSnapshot {
        plan: "MiniMax Token Plan".into(),
        session: window.clone(),
        weekly: window,
        video_session: None,
        video_weekly: None,
    };
    assert_eq!(
        minimax_vendor::build_placeholders(&minimax, now)["session_pct"],
        "25"
    );
}
