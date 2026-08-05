//! SuperGrok renderer — weekly subscription credit % + reset countdown,
//! with optional per-product rows (GrokBuild / Api) in the tooltip.

use std::collections::HashMap;

use chrono::{DateTime, Utc};

use crate::countdown;
use crate::format::{placeholders, substitute, updated_at_hm};
use crate::pacing::PaceSeverity;
use crate::pango::{self, color_span, escape, severity_color, severity_for};
use crate::theme::Theme;
use crate::tooltip::{Line as TooltipLine, render_bordered};
use crate::usage::SuperGrokSnapshot;
use crate::vendor::{RenderOpts, VendorOutcome};
use crate::waybar::{Class, WaybarOutput};

use super::fetch::FetchOutcome;

pub const DEFAULT_FORMAT: &str = "{weekly_pct}% · {weekly_reset}";

const DEFAULT_ICON: &str = "󰚩";

pub fn build_placeholders(
    snap: &SuperGrokSnapshot,
    now: DateTime<Utc>,
) -> HashMap<&'static str, String> {
    let pct = snap.weekly_pct.to_string();
    let reset = countdown::format(snap.reset_at, now);
    let prepaid = snap
        .prepaid_balance
        .map(|b| {
            if b < 0.0 {
                format!("-${:.2}", -b)
            } else {
                format!("${b:.2}")
            }
        })
        .unwrap_or_else(|| "—".into());

    placeholders(vec![
        ("icon", DEFAULT_ICON.to_string()),
        ("vendor_short", "sgk".to_string()),
        // Cross-vendor aliases: SuperGrok's headline is the weekly credit pool.
        ("plan", snap.plan.clone()),
        ("session_pct", pct.clone()),
        ("session_reset", reset.clone()),
        ("weekly_pct", pct.clone()),
        ("weekly_reset", reset.clone()),
        // SuperGrok-specific.
        ("sgk_plan", snap.plan.clone()),
        ("sgk_pct", pct),
        ("sgk_reset", reset),
        ("sgk_prepaid", prepaid),
    ])
}

pub fn severity(snap: &SuperGrokSnapshot) -> PaceSeverity {
    severity_for(snap.weekly_pct)
}

pub fn render(
    outcome: &VendorOutcome,
    snap: &SuperGrokSnapshot,
    theme: &Theme,
    opts: &RenderOpts,
    now: DateTime<Utc>,
) -> WaybarOutput {
    let class = Class::from(severity(snap));
    let format = opts
        .format
        .clone()
        .unwrap_or_else(|| DEFAULT_FORMAT.to_string());
    let mut values = build_placeholders(snap, now);
    for key in ["plan", "sgk_plan"] {
        if let Some(value) = values.get_mut(key) {
            *value = escape(value);
        }
    }

    let mut text = substitute(&format, &values);
    if outcome.stale {
        text.push_str(" ⏸");
    }

    let wrapper_color = severity_color(severity(snap), theme).to_string();
    let icon_prefix = match opts.icon.as_deref() {
        Some(ic) if !ic.is_empty() => format!("{ic} "),
        _ => String::new(),
    };
    let bar_text = color_span(&wrapper_color, &format!("{icon_prefix}{text}"));

    let tooltip = if let Some(fmt) = opts.tooltip_format.as_deref() {
        substitute(fmt, &values)
    } else {
        render_tooltip(outcome, snap, theme, now)
    };

    WaybarOutput {
        text: bar_text,
        tooltip,
        class,
    }
}

/// One usage-percent row in the same shape as `tooltip::push_window`:
/// label, progress bar + bold %, then optional dim reset line.
fn push_pct_row(
    lines: &mut Vec<TooltipLine>,
    theme: &Theme,
    label: &str,
    pct: i32,
    reset_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) {
    let fg = &theme.fg;
    let dim = &theme.dim;
    let color = severity_color(severity_for(pct), theme);
    let bar = pango::progress_bar(pct, color, theme, None);
    lines.push(TooltipLine::Body(format!(
        " <span foreground='{fg}'>{label}</span>"
    )));
    lines.push(TooltipLine::Body(format!(
        "   {bar}  <span font_weight='bold' foreground='{color}'>{pct}%</span>"
    )));
    if reset_at.is_some() {
        lines.push(TooltipLine::Body(format!(
            " <span foreground='{dim}'>  ⏱  Resets in {}</span>",
            escape(&countdown::format(reset_at, now))
        )));
    }
}

fn render_tooltip(
    outcome: &VendorOutcome,
    snap: &SuperGrokSnapshot,
    theme: &Theme,
    now: DateTime<Utc>,
) -> String {
    let blue = &theme.blue;
    let dim = &theme.dim;

    let mut lines: Vec<TooltipLine> = Vec::new();
    lines.push(TooltipLine::Center(format!(
        "<span font_weight='bold' foreground='{blue}'>{}</span>",
        escape(&snap.plan)
    )));
    lines.push(TooltipLine::Sep);
    lines.push(TooltipLine::Body("".into()));

    // Weekly headline — same layout as Anthropic/OpenAI window rows.
    push_pct_row(
        &mut lines,
        theme,
        "  󰔟  Weekly credits",
        snap.weekly_pct,
        snap.reset_at,
        now,
    );

    for product in &snap.products {
        lines.push(TooltipLine::Body("".into()));
        // Product names come from the billing API — escape before embedding.
        let label = format!("  󰚩  {}", escape(&product.name));
        push_pct_row(&mut lines, theme, &label, product.pct, None, now);
    }

    if let Some(bal) = snap.prepaid_balance {
        let bal_s = if bal < 0.0 {
            format!("-${:.2}", -bal)
        } else {
            format!("${bal:.2}")
        };
        lines.push(TooltipLine::Body("".into()));
        lines.push(TooltipLine::Body(format!(
            " <span foreground='{dim}'>  󰢗  Prepaid API  {}</span>",
            escape(&bal_s)
        )));
    }

    if let Some((code, msg)) = outcome.last_error.as_ref()
        && *code != 0
    {
        let (icon, ecolor) = if *code >= 500 {
            ("󰅚", theme.red.as_str())
        } else {
            ("󰀪", theme.orange.as_str())
        };
        lines.push(TooltipLine::Body("".into()));
        lines.push(TooltipLine::Sep);
        lines.push(TooltipLine::Body(format!(
            " <span foreground='{ecolor}'>  {icon}  HTTP {code}</span>"
        )));
        lines.push(TooltipLine::Body(format!(
            "     <span foreground='{dim}'>{}</span>",
            escape(msg)
        )));
    }

    let updated = updated_at_hm(now, outcome.cache_age);
    lines.push(TooltipLine::Body("".into()));
    lines.push(TooltipLine::Sep);
    lines.push(TooltipLine::Body(format!(
        " <span foreground='{dim}'>  󰅐  Updated {updated}</span>"
    )));

    render_bordered(&lines, theme)
}

impl From<FetchOutcome> for VendorOutcome {
    fn from(o: FetchOutcome) -> Self {
        Self {
            snapshot: crate::usage::VendorSnapshot::SuperGrok(o.snapshot),
            stale: o.stale,
            last_error: o.last_error,
            cache_age: o.cache_age,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::SuperGrokProduct;
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 5, 12, 0, 0).unwrap()
    }

    fn sample_snap() -> SuperGrokSnapshot {
        SuperGrokSnapshot {
            plan: "SuperGrok".into(),
            account: "user-1".into(),
            weekly_pct: 34,
            reset_at: Some(now() + chrono::Duration::hours(20)),
            products: vec![SuperGrokProduct {
                name: "GrokBuild".into(),
                pct: 34,
            }],
            prepaid_balance: Some(0.0),
        }
    }

    fn sample_outcome(snap: SuperGrokSnapshot) -> VendorOutcome {
        VendorOutcome {
            snapshot: crate::usage::VendorSnapshot::SuperGrok(snap),
            stale: false,
            last_error: None,
            cache_age: Some(std::time::Duration::from_secs(10)),
        }
    }

    fn opts() -> RenderOpts {
        RenderOpts {
            format: None,
            tooltip_format: None,
            icon: None,
            pace_tolerance: 5,
            format_pace_color: false,
            tooltip_pace_pts: false,
        }
    }

    #[test]
    fn renders_weekly_pct_and_reset() {
        let snap = sample_snap();
        let o = sample_outcome(snap.clone());
        let out = render(&o, &snap, &Theme::default(), &opts(), now());
        assert!(out.text.contains("34%"));
        assert!(out.tooltip.contains("Weekly credits"));
        assert!(out.tooltip.contains("GrokBuild"));
        assert!(out.tooltip.contains("SuperGrok"));
        // Usage-% vendors (Anthropic / OpenAI) draw a filled progress bar in
        // the tooltip; SuperGrok must match that shape rather than bare %.
        assert!(
            out.tooltip.contains('█') || out.tooltip.contains('░'),
            "tooltip missing progress bar cells: {}",
            out.tooltip
        );
        assert!(out.tooltip.contains("Resets in"));
    }

    #[test]
    fn high_usage_is_critical() {
        let mut snap = sample_snap();
        snap.weekly_pct = 95;
        assert_eq!(severity(&snap), PaceSeverity::Critical);
    }

    #[test]
    fn placeholders_include_generic_aliases() {
        let snap = sample_snap();
        let ph = build_placeholders(&snap, now());
        assert_eq!(ph.get("vendor_short").map(String::as_str), Some("sgk"));
        assert_eq!(ph.get("weekly_pct").map(String::as_str), Some("34"));
        assert_eq!(ph.get("session_pct").map(String::as_str), Some("34"));
        assert_eq!(ph.get("sgk_prepaid").map(String::as_str), Some("$0.00"));
    }
}
