//! Pure rendering of the status line.
//!
//! No I/O, no clock, no `git` subprocess: everything the line shows is passed
//! in. That keeps it testable and keeps the hot path free of the two things
//! that actually cost milliseconds — spawning `git` and reading transcripts.
//!
//! The visual style (Nerd Font icons, colours, the 10-cell bar, the 50/70/90
//! thresholds) and the `fish_shorten_path`, `truncate_middle`, `format_tokens`
//! and `format_cost` helpers are adapted from khoi/cc-statusline-rs (MIT).
//! See NOTICE.

use crate::ledger::LedgerSummary;
use crate::payload::StatusPayload;

const RESET: &str = "\x1b[0m";
const GREY: &str = "\x1b[90m";
const RED: &str = "\x1b[31m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const CYAN: &str = "\x1b[36m";
const ORANGE: &str = "\x1b[38;5;208m";
const BLUE: &str = "\x1b[38;5;12m";
const MAGENTA: &str = "\x1b[38;5;13m";
const TEAL: &str = "\x1b[38;5;14m";

const ICON_BRANCH: char = '\u{f02a2}';
const ICON_MODEL: char = '\u{e26d}';
const ICON_CONTEXT: char = '\u{f49b}';
const ICON_TOKENS: char = '\u{f0ec}';
const ICON_LIMITS: char = '\u{f252}';

const BAR_WIDTH: usize = 10;
const BRANCH_MAX: usize = 32;

/// Everything the line needs. `branch` and `home` are resolved by the caller so
/// this function stays pure.
pub struct RenderInput<'a> {
    pub payload: &'a StatusPayload,
    pub summary: &'a LedgerSummary,
    pub branch: Option<&'a str>,
    pub home: &'a str,
    pub short: bool,
}

pub fn render(input: &RenderInput) -> String {
    let p = input.payload;

    let mut segments: Vec<String> = Vec::new();

    if let Some(pct) = p.context_percent() {
        segments.push(context_segment(pct, input.short));
    }

    if input.short {
        // Compact variant: directory, context, today. Nothing else earns space.
        if input.summary.today_tokens > 0 {
            segments.push(format!(
                "{GREY}{ICON_TOKENS} {RESET}{}",
                format_tokens(input.summary.today_tokens as u64)
            ));
        }
        return join(&head(p, input, true), &segments);
    }

    if let Some(model) = p.model_name() {
        let effort = match p.effort_level() {
            Some(level) => format!("{GREY} ({level})"),
            None => String::new(),
        };
        segments.push(format!("{TEAL}{ICON_MODEL} {ORANGE}{model}{effort}{RESET}"));
    }

    segments.push(format!(
        "{GREY}{ICON_TOKENS} {RESET}{} {GREY}session{RESET}",
        format_tokens(input.summary.session_tokens as u64)
    ));

    segments.push(format!(
        "{GREY}today {RESET}{} {GREY}/ week {RESET}{}",
        format_tokens(input.summary.today_tokens as u64),
        format_tokens(input.summary.week_tokens as u64),
    ));

    // Absent on some accounts and gone from the payload entirely for a stretch
    // earlier this year, so its absence is normal and silent.
    if let Some(limits) = rate_limit_segment(p) {
        segments.push(limits);
    }

    join(&head(p, input, false), &segments)
}

/// The line shown when the payload could not be parsed at all. Still prints
/// something useful; the status line must never look broken.
pub fn fallback(dir: Option<&str>, home: &str) -> String {
    match dir {
        Some(dir) => format!("{CYAN}{}{RESET}", fish_shorten_path(dir, home)),
        None => format!("{GREY}cc-ledger{RESET}"),
    }
}

fn head(p: &StatusPayload, input: &RenderInput, short: bool) -> String {
    let dir = p
        .display_dir()
        .map(|d| fish_shorten_path(d, input.home))
        .unwrap_or_default();

    let branch = input.branch.unwrap_or("");
    if branch.is_empty() || short {
        return format!("{CYAN}{dir}{RESET}");
    }

    let branch = truncate_middle(branch, BRANCH_MAX);
    let changed = lines_changed(p);
    if dir.is_empty() {
        format!("{BLUE}{ICON_BRANCH} {GREEN}{branch}{changed}{RESET}")
    } else {
        format!("{CYAN}{dir}{RESET} {BLUE}{ICON_BRANCH} {GREEN}{branch}{changed}{RESET}")
    }
}

fn lines_changed(p: &StatusPayload) -> String {
    let Some(cost) = p.cost.as_ref() else {
        return String::new();
    };
    let added = cost.total_lines_added.unwrap_or(0);
    let removed = cost.total_lines_removed.unwrap_or(0);
    if added == 0 && removed == 0 {
        return String::new();
    }
    format!("({GREEN}+{added}{RESET} {RED}-{removed}{RESET})")
}

fn context_segment(pct: f64, short: bool) -> String {
    let pct = pct.clamp(0.0, 100.0);
    let colour = pct_colour(pct);
    if short {
        return format!("{colour}{}%{RESET}", pct.round() as u32);
    }
    format!(
        "{MAGENTA}{ICON_CONTEXT} {GREY}{}{RESET} {colour}{}%{RESET}",
        bar(pct),
        pct.round() as u32
    )
}

fn rate_limit_segment(p: &StatusPayload) -> Option<String> {
    let five = p.five_hour().and_then(|r| r.used_percentage);
    let seven = p.seven_day().and_then(|r| r.used_percentage);
    if five.is_none() && seven.is_none() {
        return None;
    }

    let mut parts = Vec::new();
    if let Some(v) = five {
        parts.push(format!(
            "{GREY}5h {}{}%{RESET}",
            pct_colour(v),
            v.round() as u32
        ));
    }
    if let Some(v) = seven {
        parts.push(format!(
            "{GREY}7d {}{}%{RESET}",
            pct_colour(v),
            v.round() as u32
        ));
    }
    Some(format!("{GREY}{ICON_LIMITS} {RESET}{}", parts.join(" ")))
}

fn join(head: &str, segments: &[String]) -> String {
    let sep = format!("{GREY} • {RESET}");
    let body = segments.join(&sep);
    if head.is_empty() {
        body
    } else if body.is_empty() {
        head.to_string()
    } else {
        format!("{head}{sep}{body}")
    }
}

fn pct_colour(pct: f64) -> &'static str {
    if pct >= 90.0 {
        RED
    } else if pct >= 70.0 {
        ORANGE
    } else if pct >= 50.0 {
        YELLOW
    } else {
        GREY
    }
}

fn bar(pct: f64) -> String {
    let filled = (pct * BAR_WIDTH as f64 / 100.0).round() as usize;
    let filled = filled.min(BAR_WIDTH);
    "█".repeat(filled) + &"░".repeat(BAR_WIDTH - filled)
}

// ---------------------------------------------------------------------------
// Formatting helpers, adapted from khoi/cc-statusline-rs (MIT).
// ---------------------------------------------------------------------------

/// Mirrors the reference implementation's `k` ladder, extended with an `M`
/// rung. Cache-read totals run into the hundreds of millions, and `383141k`
/// is not a number anyone reads at a glance.
pub fn format_tokens(tokens: u64) -> String {
    let k = tokens as f64 / 1000.0;
    if k >= 1000.0 {
        let m = k / 1000.0;
        if m >= 100.0 {
            format!("{}M", m.round() as u64)
        } else if m >= 10.0 {
            format!("{m:.0}M")
        } else {
            format!("{m:.1}M")
        }
    } else if k >= 100.0 {
        format!("{}k", k.round() as u64)
    } else if k >= 10.0 {
        format!("{k:.0}k")
    } else {
        format!("{k:.1}k")
    }
}

pub fn format_cost(cost: f64) -> String {
    if cost < 0.01 {
        format!("{cost:.3}")
    } else {
        format!("{cost:.2}")
    }
}

pub fn truncate_middle(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        return s.to_string();
    }
    let keep = max - 1;
    let head = keep - keep / 2;
    let tail = keep / 2;
    let mut out: String = chars[..head].iter().collect();
    out.push('…');
    out.extend(&chars[chars.len() - tail..]);
    out
}

/// `/home/me/projects/Tools/cc-ledger` → `~/p/T/cc-ledger`.
pub fn fish_shorten_path(path: &str, home: &str) -> String {
    let path = if !home.is_empty() && path.starts_with(home) {
        path.replacen(home, "~", 1)
    } else {
        path.to_string()
    };

    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() <= 1 {
        return path;
    }

    let last = parts.len() - 1;
    let shortened: Vec<String> = parts
        .iter()
        .enumerate()
        .map(|(i, part)| {
            if i == last || part.is_empty() || *part == "~" {
                (*part).to_string()
            } else if part.starts_with('.') && part.len() > 1 {
                format!(".{}", part.chars().nth(1).unwrap_or_default())
            } else {
                part.chars().next().map(String::from).unwrap_or_default()
            }
        })
        .collect();

    shortened.join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Assertions read better against the text, not the escape codes.
    fn strip_ansi(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                for c in chars.by_ref() {
                    if c == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    fn payload(json: &str) -> StatusPayload {
        StatusPayload::parse(json).expect("fixture parses")
    }

    fn summary() -> LedgerSummary {
        LedgerSummary {
            session_tokens: 12_345,
            today_tokens: 250_000,
            week_tokens: 1_200_000,
        }
    }

    const FULL: &str = include_str!("../tests/fixtures/payload_full.json");

    fn input<'a>(
        p: &'a StatusPayload,
        s: &'a LedgerSummary,
        branch: Option<&'a str>,
        short: bool,
    ) -> RenderInput<'a> {
        RenderInput {
            payload: p,
            summary: s,
            branch,
            home: "/home/user",
            short,
        }
    }

    #[test]
    fn renders_every_segment_from_a_full_payload() {
        let p = payload(FULL);
        let s = summary();
        let line = render(&input(&p, &s, Some("main"), false));
        let text = strip_ansi(&line);

        assert!(text.contains("~/p/example"), "shortened path: {text}");
        assert!(text.contains("main"), "branch: {text}");
        assert!(text.contains("(+188 -10)"), "lines changed: {text}");
        assert!(text.contains("Opus 5"), "model: {text}");
        assert!(text.contains("(high)"), "effort: {text}");
        assert!(text.contains("38%"), "context percent: {text}");
        // 12 345 tokens is 12.345k, which crosses the 10k threshold into
        // whole-k formatting.
        assert!(text.contains("12k session"), "session tokens: {text}");
        assert!(text.contains("250k"), "today tokens: {text}");
        assert!(text.contains("1.2M"), "week tokens: {text}");
        assert!(text.contains("5h 13%"), "five hour limit: {text}");
        assert!(text.contains("7d 44%"), "seven day limit: {text}");
    }

    #[test]
    fn short_variant_drops_the_verbose_segments() {
        let p = payload(FULL);
        let s = summary();
        let long = render(&input(&p, &s, Some("main"), false));
        let short = render(&input(&p, &s, Some("main"), true));

        assert!(strip_ansi(&short).len() < strip_ansi(&long).len());
        assert!(!strip_ansi(&short).contains("Opus 5"));
        assert!(!strip_ansi(&short).contains("session"));
        assert!(strip_ansi(&short).contains("38%"));
    }

    /// The case that must never panic: Claude Code sent us almost nothing.
    #[test]
    fn renders_from_an_empty_payload() {
        let p = payload("{}");
        let s = LedgerSummary::default();
        let line = render(&input(&p, &s, None, false));
        let text = strip_ansi(&line);

        assert!(!text.contains("null"));
        assert!(text.contains("0.0k"), "zeroes still render: {text}");
    }

    #[test]
    fn omits_rate_limits_when_the_payload_has_none() {
        let p = payload(r#"{"cwd":"/home/user/x"}"#);
        let s = LedgerSummary::default();
        let text = strip_ansi(&render(&input(&p, &s, None, false)));
        assert!(!text.contains("5h"), "no rate limit segment: {text}");
        assert!(!text.contains("7d"), "no rate limit segment: {text}");
    }

    #[test]
    fn omits_lines_changed_when_nothing_changed() {
        let p = payload(
            r#"{"cwd":"/home/user/x","cost":{"total_lines_added":0,"total_lines_removed":0}}"#,
        );
        let s = LedgerSummary::default();
        let text = strip_ansi(&render(&input(&p, &s, Some("main"), false)));
        assert!(!text.contains('('), "no empty parens: {text}");
    }

    #[test]
    fn context_bar_tracks_the_percentage() {
        assert_eq!(bar(0.0), "░░░░░░░░░░");
        assert_eq!(bar(50.0), "█████░░░░░");
        assert_eq!(bar(100.0), "██████████");
        assert_eq!(bar(150.0), "██████████", "clamped, never wider");
    }

    #[test]
    fn colour_thresholds_match_the_reference() {
        assert_eq!(pct_colour(0.0), GREY);
        assert_eq!(pct_colour(49.9), GREY);
        assert_eq!(pct_colour(50.0), YELLOW);
        assert_eq!(pct_colour(69.9), YELLOW);
        assert_eq!(pct_colour(70.0), ORANGE);
        assert_eq!(pct_colour(89.9), ORANGE);
        assert_eq!(pct_colour(90.0), RED);
    }

    #[test]
    fn format_tokens_switches_precision_by_magnitude() {
        assert_eq!(format_tokens(0), "0.0k");
        assert_eq!(format_tokens(1_234), "1.2k");
        assert_eq!(format_tokens(12_340), "12k");
        assert_eq!(format_tokens(250_000), "250k");
        assert_eq!(format_tokens(999_499), "999k", "last rung below M");
        assert_eq!(format_tokens(1_000_000), "1.0M", "first rung at M");
        assert_eq!(format_tokens(1_200_000), "1.2M");
        assert_eq!(format_tokens(15_000_000), "15M");
        assert_eq!(format_tokens(383_141_000), "383M", "a real week's total");
    }

    #[test]
    fn format_cost_keeps_small_amounts_readable() {
        assert_eq!(format_cost(0.0), "0.000");
        assert_eq!(format_cost(0.005), "0.005");
        assert_eq!(format_cost(12.5), "12.50");
    }

    #[test]
    fn truncate_middle_caps_long_branches() {
        assert_eq!(truncate_middle("main", 32), "main");
        let long = truncate_middle("khoi/goo-3197-cursor-smoothing-diagnostics", 32);
        assert_eq!(long, "khoi/goo-3197-cu…ing-diagnostics");
        assert_eq!(long.chars().count(), 32);
    }

    #[test]
    fn fish_shorten_path_abbreviates_all_but_the_last_segment() {
        assert_eq!(
            fish_shorten_path("/home/user/projects/Tools/cc-ledger", "/home/user"),
            "~/p/T/cc-ledger"
        );
        assert_eq!(fish_shorten_path("/home/user", "/home/user"), "~");
        assert_eq!(fish_shorten_path("/tmp", ""), "/tmp");
    }

    #[test]
    fn fish_shorten_path_keeps_dotdirs_legible() {
        assert_eq!(
            fish_shorten_path("/home/user/.config/nvim", "/home/user"),
            "~/.c/nvim"
        );
    }

    #[test]
    fn fallback_line_is_never_empty() {
        assert!(!fallback(None, "/home/user").is_empty());
        assert!(strip_ansi(&fallback(Some("/home/user/x"), "/home/user")).contains("~/x"));
    }
}
