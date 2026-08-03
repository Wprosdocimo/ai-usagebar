//! GitHub Copilot renderer — a single premium-request pool, same shape as
//! `kiro::vendor` (one headline %) with an "unlimited" branch like `cursor::vendor`.

use std::collections::HashMap;

use chrono::{DateTime, Utc};

use crate::countdown;
use crate::format::{placeholders, substitute, updated_at_hm};
use crate::pacing::PaceSeverity;
use crate::pango::{color_span, escape, severity_color, severity_for};
use crate::theme::Theme;
use crate::tooltip::{Line as TooltipLine, render_bordered};
use crate::usage::CopilotSnapshot;
use crate::vendor::{RenderOpts, VendorOutcome};
use crate::waybar::{Class, WaybarOutput};

use super::fetch::FetchOutcome;

pub const DEFAULT_FORMAT: &str = "{copilot_pct}%";

/// GitHub glyph (nf-fa-github, U+F09B), the closest common Nerd Font icon to
/// an official Copilot mark. Written as an escape, not the literal char:
/// private-use-area glyphs get silently eaten by some editors/tools — that is
/// how this constant ended up empty once. Override with `--icon`.
const DEFAULT_ICON: &str = "\u{F09B}";

fn count(v: f64) -> String {
    if (v.fract()).abs() < f64::EPSILON {
        format!("{v:.0}")
    } else {
        format!("{v:.1}")
    }
}

pub fn build_placeholders(
    snap: &CopilotSnapshot,
    now: DateTime<Utc>,
) -> HashMap<&'static str, String> {
    let pct = snap.pct();
    let reset = countdown::format(snap.reset_at, now);
    placeholders(vec![
        ("icon", DEFAULT_ICON.to_string()),
        ("vendor_short", "cop".to_string()),
        // Cross-vendor aliases: one pool, so it fills both generic slots.
        ("plan", "GitHub Copilot".to_string()),
        ("session_pct", pct.to_string()),
        ("session_reset", reset.clone()),
        ("weekly_pct", pct.to_string()),
        ("weekly_reset", reset.clone()),
        // Copilot-specific placeholders.
        ("copilot_pct", pct.to_string()),
        ("copilot_used", count(snap.used())),
        ("copilot_entitlement", count(snap.entitlement)),
        ("copilot_remaining", count(snap.remaining)),
        ("copilot_reset", reset),
        (
            "copilot_unlimited",
            if snap.unlimited { "yes" } else { "no" }.to_string(),
        ),
    ])
}

pub fn severity(snap: &CopilotSnapshot) -> PaceSeverity {
    if snap.unlimited {
        PaceSeverity::Low
    } else {
        severity_for(snap.pct())
    }
}

pub fn render(
    outcome: &VendorOutcome,
    snap: &CopilotSnapshot,
    theme: &Theme,
    opts: &RenderOpts,
    now: DateTime<Utc>,
) -> WaybarOutput {
    let class = Class::from(severity(snap));
    let format = opts
        .format
        .clone()
        .unwrap_or_else(|| DEFAULT_FORMAT.to_string());
    let values = build_placeholders(snap, now);

    let mut text = if snap.unlimited && opts.format.is_none() {
        "unlimited".to_string()
    } else {
        substitute(&format, &values)
    };
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

fn render_tooltip(
    outcome: &VendorOutcome,
    snap: &CopilotSnapshot,
    theme: &Theme,
    now: DateTime<Utc>,
) -> String {
    let blue = &theme.blue;
    let dim = &theme.dim;
    let fg = &theme.fg;

    let mut lines: Vec<TooltipLine> = Vec::new();
    lines.push(TooltipLine::Center(format!(
        "<span font_weight='bold' foreground='{blue}'>GitHub Copilot</span>"
    )));
    lines.push(TooltipLine::Sep);
    lines.push(TooltipLine::Body("".into()));

    if snap.unlimited {
        lines.push(TooltipLine::Body(format!(
            " <span foreground='{fg}'>  󰐾  Unlimited premium requests</span>"
        )));
    } else {
        let pct = snap.pct();
        let color = severity_color(severity_for(pct), theme);
        lines.push(TooltipLine::Body(format!(
            " <span foreground='{fg}'>  󰚩  Premium requests</span>"
        )));
        lines.push(TooltipLine::Body(format!(
            "   <span font_weight='bold' foreground='{color}'>{pct}%</span> used \
             <span foreground='{dim}'>({} of {})</span>",
            count(snap.used()),
            count(snap.entitlement)
        )));
    }

    lines.push(TooltipLine::Body("".into()));
    lines.push(TooltipLine::Body(format!(
        " <span foreground='{dim}'>  󰃰  Resets {}</span>",
        escape(&countdown::format(snap.reset_at, now))
    )));

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
            snapshot: crate::usage::VendorSnapshot::Copilot(o.snapshot),
            stale: o.stale,
            last_error: o.last_error,
            cache_age: o.cache_age,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, 26, 12, 0, 0).unwrap()
    }

    fn sample_snap() -> CopilotSnapshot {
        CopilotSnapshot {
            entitlement: 300.0,
            remaining: 40.0,
            unlimited: false,
            reset_at: Some(now() + chrono::Duration::days(3)),
        }
    }

    fn sample_outcome(snap: CopilotSnapshot) -> VendorOutcome {
        VendorOutcome {
            snapshot: crate::usage::VendorSnapshot::Copilot(snap),
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
    fn default_bar_shows_the_percentage() {
        let snap = sample_snap();
        let out = render(
            &sample_outcome(snap.clone()),
            &snap,
            &Theme::default(),
            &opts(),
            now(),
        );
        assert!(out.text.contains("87%"), "text: {}", out.text); // 260/300
    }

    #[test]
    fn tooltip_shows_premium_requests_and_reset() {
        let snap = sample_snap();
        let out = render(
            &sample_outcome(snap.clone()),
            &snap,
            &Theme::default(),
            &opts(),
            now(),
        );
        assert!(out.tooltip.contains("GitHub Copilot"));
        assert!(out.tooltip.contains("87%"));
        assert!(out.tooltip.contains("260"));
        assert!(out.tooltip.contains("300"));
        assert!(out.tooltip.contains("3d"));
    }

    #[test]
    fn unlimited_plan_is_calm_and_labeled() {
        let mut snap = sample_snap();
        snap.unlimited = true;
        let out = render(
            &sample_outcome(snap.clone()),
            &snap,
            &Theme::default(),
            &opts(),
            now(),
        );
        assert_eq!(severity(&snap), PaceSeverity::Low);
        assert!(out.text.contains("unlimited"));
        assert!(out.tooltip.contains("Unlimited premium requests"));
    }

    #[test]
    fn stale_appends_pause() {
        let snap = sample_snap();
        let mut outcome = sample_outcome(snap.clone());
        outcome.stale = true;
        let out = render(&outcome, &snap, &Theme::default(), &opts(), now());
        assert!(out.text.contains("⏸"));
    }

    #[test]
    fn custom_tooltip_uses_placeholders() {
        let snap = sample_snap();
        let mut o = opts();
        o.tooltip_format = Some("{copilot_used}/{copilot_entitlement}".into());
        let out = render(
            &sample_outcome(snap.clone()),
            &snap,
            &Theme::default(),
            &o,
            now(),
        );
        assert_eq!(out.tooltip, "260/300");
    }

    #[test]
    fn generic_windows_map_to_the_single_pool() {
        let values = build_placeholders(&sample_snap(), now());
        assert_eq!(values["session_pct"], "87");
        assert_eq!(values["weekly_pct"], "87");
        assert_eq!(values["plan"], "GitHub Copilot");
    }

    #[test]
    fn placeholder_set_contains_all_keys() {
        let values = build_placeholders(&sample_snap(), now());
        for key in [
            "icon",
            "vendor_short",
            "plan",
            "session_pct",
            "session_reset",
            "weekly_pct",
            "weekly_reset",
            "copilot_pct",
            "copilot_used",
            "copilot_entitlement",
            "copilot_remaining",
            "copilot_reset",
            "copilot_unlimited",
        ] {
            assert!(values.contains_key(key), "missing placeholder {key}");
        }
    }

    /// The U+F09B glyph was silently stripped once (see the constant's
    /// comment) — pin it non-empty so a tool eating the PUA char again fails
    /// loudly instead of shipping an invisible `{icon}`.
    #[test]
    fn default_icon_placeholder_is_not_empty() {
        let values = build_placeholders(&sample_snap(), now());
        assert!(!values["icon"].is_empty());
    }

    #[test]
    fn fetch_outcome_conversion_preserves_metadata() {
        let fetch = FetchOutcome {
            snapshot: sample_snap(),
            stale: true,
            last_error: Some((401, "bad".into())),
            cache_age: Some(std::time::Duration::from_secs(42)),
        };
        let vendor: VendorOutcome = fetch.into();
        assert!(matches!(
            vendor.snapshot,
            crate::usage::VendorSnapshot::Copilot(_)
        ));
        assert!(vendor.stale);
    }
}
