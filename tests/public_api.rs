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

/// The macOS menu bar reads Z.AI's MCP pool through these three placeholders in
/// its `--format` string (`macos/ai-usagebar-menubar.swift`, fields 31-33).
/// Renaming one here would silently drop that row, so pin the names — and pin
/// the `—` an absent pool reports, which is the presence signal the Swift
/// parser keys off to avoid painting a phantom 0% row.
#[test]
fn the_zai_mcp_placeholders_the_macos_menu_bar_reads_keep_their_names() {
    let now = Utc.with_ymd_and_hms(2026, 8, 19, 12, 0, 0).unwrap();
    let mcp = UsageWindow {
        utilization_pct: 7,
        resets_at: Some(now + chrono::Duration::days(24)),
        window_duration: chrono::Duration::days(30),
    };
    let with_mcp = ZaiSnapshot {
        plan: "GLM Coding Pro".into(),
        session: None,
        weekly: None,
        mcp: Some(mcp),
    };
    let v = zai_vendor::build_placeholders(&with_mcp, now);
    assert_eq!(v["zai_mcp_pct"], "7");
    assert_eq!(v["zai_mcp_reset"], "24d 0h");
    assert_eq!(v["zai_mcp_elapsed"], "20");

    let without_mcp = ZaiSnapshot {
        mcp: None,
        ..with_mcp
    };
    let v = zai_vendor::build_placeholders(&without_mcp, now);
    assert_eq!(
        v["zai_mcp_reset"], "—",
        "an absent pool must stay distinguishable from a real 0% one"
    );
}
