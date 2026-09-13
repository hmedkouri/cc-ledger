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

/// Everything the line needs. `branch`, `home` and `now` are resolved by the
/// caller so this function stays pure — in particular the reset countdown is
/// derived from `now`, never from the clock, so rendering is deterministic and
/// testable at any instant.
pub struct RenderInput<'a> {
    pub payload: &'a StatusPayload,
    pub summary: &'a LedgerSummary,
    pub branch: Option<&'a str>,
    pub home: &'a str,
    pub short: bool,
    /// Unix seconds. Only used to turn `resets_at` into time remaining.
    pub now: i64,
    /// Render today as API-equivalent dollars rather than tokens.
    pub cost: bool,
}

/// Order is deliberate: directory, context, limits, model, today.
///
/// Context and limits are the two "should I stop soon" signals, so they sit
/// together. Today is the least actionable segment and goes last, because a
/// narrow pane clips from the right and that is the thing worth losing.
pub fn render(input: &RenderInput) -> String {
    let p = input.payload;

    let mut segments: Vec<String> = Vec::new();

    if let Some(pct) = p.context_percent() {
        segments.push(context_segment(pct, input.short));
    }

    if input.short {
        // Compact variant: directory, context, today. Nothing else earns space.
        if input.summary.today_tokens > 0 {
            segments.push(today_segment(input));
        }
        return join(&head(p, input, true), &segments);
    }

    // Absent on some accounts and gone from the payload entirely for a stretch
    // earlier this year, so its absence is normal and silent.
    if let Some(limits) = rate_limit_segment(p, input.now) {
        segments.push(limits);
    }

    if let Some(model) = model_segment(p) {
        segments.push(model);
    }

    segments.push(today_segment(input));

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

fn rate_limit_segment(p: &StatusPayload, now: i64) -> Option<String> {
    let five = p.five_hour();
    let seven = p.seven_day();
    if five.and_then(|r| r.used_percentage).is_none()
        && seven.and_then(|r| r.used_percentage).is_none()
    {
        return None;
    }

    let mut parts = Vec::new();
    for (label, limit) in [("5h", five), ("7d", seven)] {
        let Some(limit) = limit else { continue };
        let Some(pct) = limit.used_percentage else {
            continue;
        };
        let until = match countdown(limit.resets_at, now) {
            Some(remaining) => format!("{GREY} ({remaining})"),
            None => String::new(),
        };
        parts.push(format!(
            "{GREY}{label} {}{}%{until}{RESET}",
            pct_colour(pct),
            pct.round() as u32
        ));
    }
    Some(format!("{GREY}{ICON_LIMITS} {RESET}{}", parts.join(" ")))
}

/// Time until `resets_at`: `2h10m` below a day, `3d` at or above one.
///
/// `None` when the field is absent or the moment has passed. A payload keeps a
/// stale `resets_at` until its next refresh, and counting down to something
/// that already happened is worse than saying nothing.
fn countdown(resets_at: Option<i64>, now: i64) -> Option<String> {
    let remaining = resets_at?.checked_sub(now)?;
    if remaining <= 0 {
        return None;
    }
    if remaining >= 86_400 {
        return Some(format!("{}d", remaining / 86_400));
    }
    let hours = remaining / 3_600;
    let minutes = (remaining % 3_600) / 60;
    Some(if hours > 0 {
        format!("{hours}h{minutes}m")
    } else {
        format!("{minutes}m")
    })
}

/// `Opus 5 · 1M · high` — name, context window, effort, each omitted if absent.
fn model_segment(p: &StatusPayload) -> Option<String> {
    let name = strip_parenthetical(p.model_name()?);

    let mut extras = Vec::new();
    if let Some(size) = p
        .context_window
        .as_ref()
        .and_then(|c| c.context_window_size)
    {
        extras.push(format_window(size));
    }
    if let Some(level) = p.effort_level() {
        extras.push(level.to_string());
    }

    let mut out = format!("{TEAL}{ICON_MODEL} {ORANGE}{name}{RESET}");
    for extra in extras {
        out.push_str(&format!("{GREY} · {extra}{RESET}"));
    }
    Some(out)
}

/// `"Opus 5 (1M context)"` → `"Opus 5"`. The window is rendered as its own
/// part, so leaving it in the display name would print it twice.
fn strip_parenthetical(name: &str) -> &str {
    match name.find('(') {
        Some(i) => name[..i].trim_end(),
        None => name,
    }
}

/// `1000000` → `1M`, `200000` → `200k`.
fn format_window(tokens: u64) -> String {
    if tokens >= 1_000_000 && tokens.is_multiple_of(1_000_000) {
        format!("{}M", tokens / 1_000_000)
    } else if tokens >= 1_000 {
        format!("{}k", tokens / 1_000)
    } else {
        tokens.to_string()
    }
}

fn today_segment(input: &RenderInput) -> String {
    let value = if input.cost {
        format!("${}", format_cost(input.summary.today_cost))
    } else {
        format_tokens(input.summary.today_tokens as u64)
    };
    format!("{GREY}{ICON_TOKENS} today {RESET}{value}")
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
            today_tokens: 250_000,
            today_cost: 41.20,
        }
    }

    const FULL: &str = include_str!("../tests/fixtures/payload_full.json");

    /// Chosen relative to the fixture's `resets_at` values so the countdowns
    /// land on exact, readable boundaries: 2h10m to the five-hour reset and
    /// 11 days to the seven-day one.
    const NOW: i64 = 1_788_608_452;

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
            now: NOW,
            cost: false,
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
        assert!(text.contains("38%"), "context percent: {text}");
        assert!(text.contains("5h 13% (2h10m)"), "five hour limit: {text}");
        assert!(text.contains("7d 44% (11d)"), "seven day limit: {text}");
        assert!(
            text.contains("Opus 5 · 200k · high"),
            "model segment: {text}"
        );
        assert!(text.contains("today 250k"), "today tokens: {text}");

        // Dropped from the default line; still available in `cc-usage`.
        assert!(!text.contains("session"), "session is gone: {text}");
        assert!(!text.contains("week"), "week is gone: {text}");
    }

    /// The order is the whole point of the layout: the two "should I stop"
    /// signals adjacent, and the least actionable segment last so that is what
    /// a narrow pane clips.
    #[test]
    fn segments_render_in_the_documented_order() {
        let p = payload(FULL);
        let s = summary();
        let text = strip_ansi(&render(&input(&p, &s, Some("main"), false)));

        let at = |needle: &str| {
            text.find(needle)
                .unwrap_or_else(|| panic!("missing {needle} in {text}"))
        };
        assert!(at("main") < at("38%"), "branch before context: {text}");
        assert!(at("38%") < at("5h"), "context before limits: {text}");
        assert!(at("5h") < at("Opus 5"), "limits before model: {text}");
        assert!(at("Opus 5") < at("today"), "model before today: {text}");
    }

    #[test]
    fn countdown_formats_and_omits_correctly() {
        let now = 1_000_000;
        assert_eq!(countdown(None, now), None, "absent resets_at");
        assert_eq!(
            countdown(Some(now), now),
            None,
            "this instant is not future"
        );
        assert_eq!(countdown(Some(now - 1), now), None, "already reset");

        assert_eq!(
            countdown(Some(now + 86_400), now),
            Some("1d".into()),
            "exactly 24h reads as days"
        );
        assert_eq!(
            countdown(Some(now + 86_399), now),
            Some("23h59m".into()),
            "one second under 24h still reads as hours"
        );
        assert_eq!(countdown(Some(now + 7_800), now), Some("2h10m".into()));
        assert_eq!(
            countdown(Some(now + 600), now),
            Some("10m".into()),
            "under an hour drops the hours part"
        );
        assert_eq!(countdown(Some(now + 3 * 86_400), now), Some("3d".into()));
    }

    #[test]
    fn model_segment_omits_absent_parts_and_never_repeats_the_window() {
        let s = summary();

        let full = payload(
            r#"{"model":{"display_name":"Opus 5 (1M context)"},
                "context_window":{"context_window_size":1000000},
                "effort":{"level":"high"}}"#,
        );
        let text = strip_ansi(&render(&input(&full, &s, None, false)));
        assert!(text.contains("Opus 5 · 1M · high"), "{text}");
        assert!(
            !text.contains("1M context"),
            "the parenthetical is stripped so the window appears once: {text}"
        );

        let bare = payload(r#"{"model":{"display_name":"Opus 5"}}"#);
        let text = strip_ansi(&render(&input(&bare, &s, None, false)));
        assert!(text.contains("Opus 5"), "{text}");
        assert!(
            !text.contains(" · "),
            "no separators with nothing to separate: {text}"
        );
    }

    #[test]
    fn window_sizes_render_compactly() {
        assert_eq!(format_window(1_000_000), "1M");
        assert_eq!(format_window(200_000), "200k");
        assert_eq!(format_window(999), "999");
        assert_eq!(strip_parenthetical("Opus 5 (1M context)"), "Opus 5");
        assert_eq!(strip_parenthetical("Opus 5"), "Opus 5");
    }

    #[test]
    fn cost_flag_renders_today_as_dollars() {
        let p = payload(FULL);
        let s = summary();
        let mut with_cost = input(&p, &s, Some("main"), false);
        with_cost.cost = true;

        let text = strip_ansi(&render(&with_cost));
        assert!(text.contains("today $41.20"), "{text}");
        assert!(
            !text.contains("250k"),
            "dollars replace tokens rather than joining them: {text}"
        );
    }

    #[test]
    fn short_variant_drops_the_verbose_segments() {
        let p = payload(FULL);
        let s = summary();
        let long = render(&input(&p, &s, Some("main"), false));
        let short = render(&input(&p, &s, Some("main"), true));

        let short = strip_ansi(&short);
        assert!(short.len() < strip_ansi(&long).len());
        assert!(!short.contains("Opus 5"), "no model segment: {short}");
        assert!(!short.contains("5h"), "no rate limits: {short}");
        assert!(
            !short.contains('█') && !short.contains('░'),
            "the bar is dropped but the percentage stays: {short}"
        );
        assert!(short.contains("38%"), "{short}");
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
