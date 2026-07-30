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
//! vendor's snapshot into labelled metrics carrying both the percentage and
//! the reset. So this file only enumerates, flattens, and formats — no vendor
//! ever needs to know it exists.

use chrono::Utc;
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
    metrics: Vec<Metric>,
    error: Option<String>,
}

struct Metric {
    label: String,
    pct: u16,
    value: String,
    detail: String,
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
    i32::from(entries.iter().all(|entry| entry.error.is_some()))
}

async fn entry_for(client: &reqwest::Client, config: &Config, tab: &TabId) -> Entry {
    let state = refresh_one(client, config, tab).await;
    let mut entry = Entry {
        id: tab_id(tab),
        name: tab_name(tab),
        plan: None,
        metrics: Vec::new(),
        error: match &state {
            TabState::Error(message) => Some(message.clone()),
            _ => None,
        },
    };
    for section in sections_for(&state, Utc::now(), PACE_TOLERANCE) {
        match section {
            Section::Title { left, .. } => entry.plan = Some(left),
            Section::Metric {
                label,
                pct,
                value_label,
                footnote,
                ..
            } => entry.metrics.push(Metric {
                label,
                pct,
                value: value_label,
                detail: footnote,
            }),
            // Balance-only vendors report through Text rows rather than a
            // gauge; keep them, since a credit balance is the whole answer
            // for those.
            Section::Text { label, value } => entry.metrics.push(Metric {
                label,
                pct: 0,
                value,
                detail: String::new(),
            }),
            Section::Block { .. } | Section::Spacer => {}
        }
    }
    entry
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
            json!({
                "id": entry.id,
                "name": entry.name,
                "plan": entry.plan,
                "error": entry.error,
                "metrics": entry.metrics.iter().map(|metric| json!({
                    "label": metric.label,
                    "percent": metric.pct,
                    "value": metric.value,
                    "detail": metric.detail,
                })).collect::<Vec<_>>(),
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
        .flat_map(|entry| entry.metrics.iter())
        .map(|metric| metric.label.chars().count())
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
        if entry.metrics.is_empty() {
            out.push_str("  (nothing reported)\n\n");
            continue;
        }
        for metric in &entry.metrics {
            let label = format!("{:width$}", metric.label, width = width);
            let value = format!("{:>9}", metric.value);
            if metric.detail.is_empty() {
                out.push_str(&format!("  {label}  {value}\n"));
            } else {
                out.push_str(&format!("  {label}  {value}   {}\n", metric.detail));
            }
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vendor::VendorId;

    fn entry(name: &str, metrics: Vec<Metric>) -> Entry {
        Entry {
            id: name.into(),
            name: name.into(),
            plan: Some("Claude Max 20x".into()),
            metrics,
            error: None,
        }
    }

    fn metric(label: &str, pct: u16, value: &str, detail: &str) -> Metric {
        Metric {
            label: label.into(),
            pct,
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
}
