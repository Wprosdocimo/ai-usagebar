//! `ai-usagebar usage` — quota and time-to-reset for everything in the config,
//! in one pass.
//!
//! The widget answers "how is *this* vendor doing" one process at a time, which
//! is what a status bar needs and what a person checking on four Claude
//! accounts does not. This walks the same tab set the TUI builds — every
//! enabled vendor, plus one entry per named Claude account — and prints what
//! each one has left.
//!
//! Deliberately thin: [`crate::tui::app::tabs_from_config`] already decides
//! what is configured, [`crate::tui::app::refresh_one`] already fetches and
//! parses it, and [`crate::tui::panels::sections_for`] already projects any
//! vendor's snapshot into labelled sections carrying every reported value.
//! So this file only enumerates, projects, and formats — no vendor
//! ever needs to know it exists.

use chrono::Utc;
use serde::Serialize;
use serde_json::json;

use crate::config::Config;
use crate::tui::app::{TabId, TabState, refresh_one, tabs_from_config};
use crate::tui::panels::{Section, sections_for};

/// Matches the widget's `--pace-tolerance` default; only affects the pacing
/// note appended to a metric's detail line.
const PACE_TOLERANCE: u32 = 5;

/// One configured vendor or account.
struct Entry {
    id: String,
    name: String,
    plan: Option<String>,
    sections: Vec<ReportSection>,
    error: Option<String>,
}

/// Lossless machine-readable projection of a TUI panel row. `metrics` remains
/// available in JSON as a convenience view over only the gauge rows; callers
/// that need every reported value should consume this ordered list.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ReportSection {
    Metric {
        label: String,
        percent: u16,
        value: String,
        detail: String,
    },
    Text {
        label: String,
        value: String,
    },
    Block {
        label: String,
        body: Vec<String>,
    },
    Spacer,
}

impl ReportSection {
    fn label(&self) -> Option<&str> {
        match self {
            Self::Metric { label, .. } | Self::Text { label, .. } | Self::Block { label, .. } => {
                Some(label)
            }
            Self::Spacer => None,
        }
    }
}

pub async fn run(json: bool) -> i32 {
    let config = match Config::load() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("ai-usagebar usage: {error}");
            return 1;
        }
    };
    let client = match crate::widget::run::http_client() {
        Ok(client) => client,
        Err(error) => {
            eprintln!("ai-usagebar usage: {error}");
            return 1;
        }
    };

    let tabs = tabs_from_config(&config);
    if tabs.is_empty() {
        eprintln!(
            "ai-usagebar usage: no vendors enabled in {}",
            crate::config::config_path_hint()
        );
        return 1;
    }

    // Sequential on purpose: several of these share a per-vendor cache lock,
    // and firing every account at Anthropic at once is a good way to get
    // rate-limited for no gain on a handful of entries.
    let mut entries = Vec::with_capacity(tabs.len());
    for tab in &tabs {
        entries.push(entry_for(&client, &config, tab).await);
    }

    if json {
        println!("{}", render_json(&entries));
    } else {
        print!("{}", render_text(&entries));
    }
    report_exit_code(&entries)
}

async fn entry_for(client: &reqwest::Client, config: &Config, tab: &TabId) -> Entry {
    let state = refresh_one(client, config, tab).await;
    entry_from_state(tab, &state, Utc::now())
}

fn entry_from_state(tab: &TabId, state: &TabState, now: chrono::DateTime<Utc>) -> Entry {
    let mut entry = Entry {
        id: tab_id(tab),
        name: tab_name(tab),
        plan: None,
        sections: Vec::new(),
        error: match &state {
            TabState::Error(message) => Some(message.clone()),
            _ => None,
        },
    };
    // The error is already a first-class entry field. Do not duplicate the
    // TUI's interactive retry instructions as report data.
    if entry.error.is_some() {
        return entry;
    }
    for section in sections_for(state, now, PACE_TOLERANCE) {
        match section {
            Section::Title { left, .. } => entry.plan = Some(left),
            Section::Metric {
                label,
                pct,
                value_label,
                footnote,
                ..
            } => entry.sections.push(ReportSection::Metric {
                label,
                percent: pct,
                value: value_label,
                detail: footnote,
            }),
            Section::Text { label, value } => {
                entry.sections.push(ReportSection::Text { label, value });
            }
            Section::Block { label, body } => {
                entry.sections.push(ReportSection::Block { label, body });
            }
            Section::Spacer => entry.sections.push(ReportSection::Spacer),
        }
    }
    entry
}

fn report_exit_code(entries: &[Entry]) -> i32 {
    i32::from(entries.iter().all(|entry| entry.error.is_some()))
}

/// Stable machine id, matching the `anthropic@<label>` convention the macOS
/// menu bar already uses for accounts.
fn tab_id(tab: &TabId) -> String {
    match &tab.account {
        Some(account) => format!("{}@{account}", tab.vendor.slug()),
        None => tab.vendor.slug().to_string(),
    }
}

fn tab_name(tab: &TabId) -> String {
    match &tab.account {
        Some(account) => format!("{} · {account}", tab.vendor.slug()),
        None => tab.vendor.slug().to_string(),
    }
}

fn render_json(entries: &[Entry]) -> String {
    let rows: Vec<serde_json::Value> = entries
        .iter()
        .map(|entry| {
            let metrics = entry
                .sections
                .iter()
                .filter_map(|section| match section {
                    ReportSection::Metric {
                        label,
                        percent,
                        value,
                        detail,
                    } => Some(json!({
                        "label": label,
                        "percent": percent,
                        "value": value,
                        "detail": detail,
                    })),
                    _ => None,
                })
                .collect::<Vec<_>>();
            json!({
                "id": entry.id,
                "name": entry.name,
                "plan": entry.plan,
                "error": entry.error,
                "metrics": metrics,
                "sections": entry.sections,
            })
        })
        .collect();
    json!({ "entries": rows }).to_string()
}

fn render_text(entries: &[Entry]) -> String {
    // Widest label across every entry, so the value column lines up down the
    // whole report rather than per-section.
    let width = entries
        .iter()
        .flat_map(|entry| entry.sections.iter())
        .filter_map(ReportSection::label)
        .map(|label| label.chars().count())
        .max()
        .unwrap_or(0);

    let mut out = String::new();
    for entry in entries {
        out.push_str(&entry.name);
        if let Some(plan) = &entry.plan {
            out.push_str(&format!("   {plan}"));
        }
        out.push('\n');
        if let Some(error) = &entry.error {
            out.push_str(&format!("  ! {error}\n\n"));
            continue;
        }
        if !entry
            .sections
            .iter()
            .any(|section| !matches!(section, ReportSection::Spacer))
        {
            out.push_str("  (nothing reported)\n\n");
            continue;
        }
        let mut body = String::new();
        let mut pending_spacer = false;
        for section in &entry.sections {
            if matches!(section, ReportSection::Spacer) {
                pending_spacer |= !body.is_empty();
                continue;
            }
            if pending_spacer {
                body.push('\n');
                pending_spacer = false;
            }
            match section {
                ReportSection::Metric {
                    label,
                    value,
                    detail,
                    ..
                } => {
                    let label = format!("{label:width$}");
                    let value = format!("{value:>9}");
                    if detail.is_empty() {
                        body.push_str(&format!("  {label}  {value}\n"));
                    } else {
                        body.push_str(&format!("  {label}  {value}   {detail}\n"));
                    }
                }
                ReportSection::Text { label, value } => {
                    if label.is_empty() {
                        body.push_str(&format!("  {}\n", value.trim_start()));
                    } else if value.is_empty() {
                        body.push_str(&format!("  {label}\n"));
                    } else {
                        body.push_str(&format!("  {label:width$}  {value}\n"));
                    }
                }
                ReportSection::Block { label, body: lines } => {
                    body.push_str(&format!("  {label}\n"));
                    for line in lines {
                        body.push_str(&format!("    {line}\n"));
                    }
                }
                ReportSection::Spacer => unreachable!(),
            }
        }
        out.push_str(&body);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::ReadyTab;
    use crate::usage::{DeepseekSnapshot, OpenRouterSnapshot, VendorSnapshot};
    use crate::vendor::VendorId;

    fn entry(name: &str, sections: Vec<ReportSection>) -> Entry {
        Entry {
            id: name.into(),
            name: name.into(),
            plan: Some("Claude Max 20x".into()),
            sections,
            error: None,
        }
    }

    fn metric(label: &str, percent: u16, value: &str, detail: &str) -> ReportSection {
        ReportSection::Metric {
            label: label.into(),
            percent,
            value: value.into(),
            detail: detail.into(),
        }
    }

    #[test]
    fn accounts_get_a_stable_id_and_a_readable_name() {
        let account = TabId::account("gmail");
        assert_eq!(tab_id(&account), "anthropic@gmail");
        assert_eq!(tab_name(&account), "anthropic · gmail");

        let plain = TabId::vendor(VendorId::Cursor);
        assert_eq!(tab_id(&plain), "cursor");
        assert_eq!(tab_name(&plain), "cursor");
    }

    #[test]
    fn every_metric_reports_its_quota_and_its_reset() {
        let text = render_text(&[entry(
            "anthropic · gmail",
            vec![
                metric("Session (5h)", 29, "29%", "Resets in 0h 50m"),
                metric("Weekly (7d)", 32, "32%", "Resets in 4d 2h"),
            ],
        )]);

        assert!(
            text.contains("anthropic · gmail   Claude Max 20x"),
            "{text}"
        );
        assert!(text.contains("29%   Resets in 0h 50m"), "{text}");
        assert!(text.contains("32%   Resets in 4d 2h"), "{text}");
    }

    /// Labels are padded to one width across the whole report, so the columns
    /// still line up when a later entry has a longer label than the first.
    #[test]
    fn value_columns_align_across_entries() {
        let text = render_text(&[
            entry("a", vec![metric("S", 1, "1%", "")]),
            entry("b", vec![metric("A very long label", 2, "2%", "")]),
        ]);
        let columns: Vec<usize> = text
            .lines()
            .filter(|line| line.starts_with("  ") && line.contains('%'))
            .map(|line| line.find('%').unwrap())
            .collect();
        assert_eq!(columns.len(), 2);
        assert_eq!(columns[0], columns[1], "{text}");
    }

    /// One dead vendor must not hide the others — it reports inline and the
    /// rest still print.
    #[test]
    fn a_failing_entry_is_reported_without_dropping_the_rest() {
        let mut broken = entry("openai", Vec::new());
        broken.error = Some("credentials error: not signed in".into());
        let text = render_text(&[broken, entry("cursor", vec![metric("Auto", 5, "5%", "")])]);

        assert!(
            text.contains("! credentials error: not signed in"),
            "{text}"
        );
        assert!(text.contains("cursor"), "{text}");
        assert!(text.contains("5%"), "{text}");
    }

    #[test]
    fn json_carries_the_percentage_as_a_number() {
        let rendered = render_json(&[entry(
            "anthropic · gmail",
            vec![metric("Session (5h)", 29, "29%", "Resets in 0h 50m")],
        )]);
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        let first = &value["entries"][0];
        assert_eq!(first["plan"], "Claude Max 20x");
        assert_eq!(first["metrics"][0]["percent"], 29);
        assert_eq!(first["metrics"][0]["detail"], "Resets in 0h 50m");
        assert!(first["error"].is_null());
    }

    #[test]
    fn json_preserves_non_metric_sections_without_fabricating_percentages() {
        let rendered = render_json(&[entry(
            "openrouter",
            vec![
                metric("Credit balance", 25, "$75.00", "$25.00 used"),
                ReportSection::Spacer,
                ReportSection::Text {
                    label: "Resets".into(),
                    value: "in 9d".into(),
                },
                ReportSection::Block {
                    label: "Usage by period".into(),
                    body: vec!["today $1.00 · week $5.00".into()],
                },
            ],
        )]);
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        let first = &value["entries"][0];
        assert_eq!(first["metrics"].as_array().unwrap().len(), 1);
        assert_eq!(first["sections"][1]["type"], "spacer");
        assert_eq!(first["sections"][2]["type"], "text");
        assert!(first["sections"][2].get("percent").is_none());
        assert_eq!(first["sections"][3]["type"], "block");
        assert_eq!(first["sections"][3]["body"][0], "today $1.00 · week $5.00");
    }

    #[test]
    fn real_panel_projection_keeps_openrouter_blocks() {
        let state = TabState::Ready(Box::new(ReadyTab {
            snapshot: VendorSnapshot::Openrouter(OpenRouterSnapshot {
                label: "OR".into(),
                total_credits: 100.0,
                total_usage: 25.0,
                usage_daily: 1.0,
                usage_weekly: 5.0,
                usage_monthly: 25.0,
                is_free_tier: false,
                limit: None,
                limit_remaining: None,
            }),
            stale: false,
            last_error: None,
            fetched_at: None,
        }));
        let projected = entry_from_state(&TabId::vendor(VendorId::Openrouter), &state, Utc::now());
        assert!(projected.sections.iter().any(|section| matches!(
            section,
            ReportSection::Block { label, .. } if label == "Usage by period"
        )));
        assert!(projected.sections.iter().any(|section| matches!(
            section,
            ReportSection::Block { label, .. } if label == "Tier"
        )));
        let text = render_text(&[projected]);
        assert!(text.contains("Usage by period"), "{text}");
        assert!(
            text.contains("today $1.00 · week $5.00 · month $25.00"),
            "{text}"
        );
    }

    #[test]
    fn real_balance_text_is_not_exposed_as_a_percentage_metric() {
        let state = TabState::Ready(Box::new(ReadyTab {
            snapshot: VendorSnapshot::Deepseek(DeepseekSnapshot {
                is_available: true,
                balance: 12.5,
                granted: 2.5,
                topped_up: 10.0,
                currency: "USD".into(),
            }),
            stale: false,
            last_error: None,
            fetched_at: None,
        }));
        let projected = entry_from_state(&TabId::vendor(VendorId::Deepseek), &state, Utc::now());
        assert!(projected.sections.iter().any(|section| matches!(
            section,
            ReportSection::Text { label, value } if label == "Balance" && value == "$12.50"
        )));
        assert!(
            !projected
                .sections
                .iter()
                .any(|section| matches!(section, ReportSection::Metric { .. }))
        );
    }

    #[test]
    fn failed_entries_do_not_duplicate_tui_retry_rows() {
        let failed = entry_from_state(
            &TabId::vendor(VendorId::Openai),
            &TabState::Error("not signed in".into()),
            Utc::now(),
        );
        assert_eq!(failed.error.as_deref(), Some("not signed in"));
        assert!(failed.sections.is_empty());
    }

    #[test]
    fn exit_is_nonzero_only_when_every_entry_failed() {
        let mut failed = entry("openai", Vec::new());
        failed.error = Some("not signed in".into());
        assert_eq!(report_exit_code(&[failed]), 1);

        let mut failed = entry("openai", Vec::new());
        failed.error = Some("not signed in".into());
        assert_eq!(report_exit_code(&[failed, entry("cursor", Vec::new())]), 0);
    }
}
