//! Typed view of the status-line JSON payload Claude Code writes to our stdin.
//!
//! Shape verified against Claude Code 2.1.270 — see `docs/formats.md` §3.
//!
//! Two rules hold everywhere in this module:
//!
//! * every field is `Option<T>`, because the payload varies by version, by
//!   model (`effort` appears only on effort-capable models) and by account
//!   state (`rate_limits` disappeared from the payload for a stretch earlier
//!   this year — anthropics/claude-code#45133, so it must degrade to nothing
//!   rather than error);
//! * unknown fields are ignored — never `deny_unknown_fields`. A new Claude
//!   Code release adding a key must not break the status line.

use serde::Deserialize;

#[derive(Debug, Default, Clone, Deserialize)]
pub struct StatusPayload {
    pub session_id: Option<String>,
    pub transcript_path: Option<String>,
    pub cwd: Option<String>,
    pub model: Option<Model>,
    pub workspace: Option<Workspace>,
    pub cost: Option<Cost>,
    pub context_window: Option<ContextWindow>,
    pub effort: Option<Effort>,
    pub rate_limits: Option<RateLimits>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct Model {
    pub id: Option<String>,
    pub display_name: Option<String>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct Workspace {
    pub current_dir: Option<String>,
    pub project_dir: Option<String>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct Cost {
    pub total_cost_usd: Option<f64>,
    pub total_lines_added: Option<u64>,
    pub total_lines_removed: Option<u64>,
}

/// Claude Code computes `used_percentage` for us; there is no need to re-derive
/// it from `current_usage` the way the reference implementation does.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct ContextWindow {
    pub context_window_size: Option<u64>,
    pub used_percentage: Option<f64>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct Effort {
    pub level: Option<String>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct RateLimits {
    pub five_hour: Option<RateLimit>,
    pub seven_day: Option<RateLimit>,
}

/// `resets_at` is Unix **seconds** (Claude Code multiplies it by 1000 itself).
#[derive(Debug, Default, Clone, Copy, PartialEq, Deserialize)]
pub struct RateLimit {
    pub used_percentage: Option<f64>,
    pub resets_at: Option<i64>,
}

impl StatusPayload {
    /// Never fails loudly: a malformed payload yields `None` and the caller
    /// prints a fallback line. The status line must not break Claude Code.
    pub fn parse(input: &str) -> Option<Self> {
        serde_json::from_str(input).ok()
    }

    /// The directory to display. `workspace.current_dir` is the authoritative
    /// one; top-level `cwd` is the fallback for older payloads.
    pub fn display_dir(&self) -> Option<&str> {
        self.workspace
            .as_ref()
            .and_then(|w| w.current_dir.as_deref())
            .or(self.cwd.as_deref())
    }

    /// Directory used to attribute ledger rows, matching the `cwd` recorded on
    /// transcript records.
    pub fn project_dir(&self) -> Option<&str> {
        self.workspace
            .as_ref()
            .and_then(|w| w.project_dir.as_deref())
            .or_else(|| self.display_dir())
    }

    pub fn model_name(&self) -> Option<&str> {
        self.model
            .as_ref()
            .and_then(|m| m.display_name.as_deref().or(m.id.as_deref()))
    }

    pub fn effort_level(&self) -> Option<&str> {
        self.effort.as_ref().and_then(|e| e.level.as_deref())
    }

    pub fn context_percent(&self) -> Option<f64> {
        self.context_window.as_ref().and_then(|c| c.used_percentage)
    }

    pub fn five_hour(&self) -> Option<RateLimit> {
        self.rate_limits.as_ref().and_then(|r| r.five_hour)
    }

    pub fn seven_day(&self) -> Option<RateLimit> {
        self.rate_limits.as_ref().and_then(|r| r.seven_day)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = include_str!("../tests/fixtures/payload_full.json");
    const MINIMAL: &str = include_str!("../tests/fixtures/payload_minimal.json");

    #[test]
    fn parses_full_payload() {
        let p = StatusPayload::parse(FULL).expect("full payload parses");
        assert_eq!(p.display_dir(), Some("/home/user/projects/example"));
        assert_eq!(p.model_name(), Some("Opus 5"));
        assert_eq!(p.effort_level(), Some("high"));
        assert_eq!(p.context_percent(), Some(37.5));
        assert_eq!(p.five_hour().and_then(|r| r.used_percentage), Some(12.5));
        assert_eq!(p.seven_day().and_then(|r| r.resets_at), Some(1789616252));
        assert!(p.transcript_path.is_some());
    }

    /// The payload that matters most: one where almost everything is missing.
    #[test]
    fn parses_minimal_payload_without_panicking() {
        let p = StatusPayload::parse(MINIMAL).expect("minimal payload parses");
        assert_eq!(p.display_dir(), Some("/tmp"));
        assert_eq!(p.model_name(), None);
        assert_eq!(p.effort_level(), None);
        assert_eq!(p.context_percent(), None);
        assert_eq!(p.five_hour(), None);
        assert_eq!(p.seven_day(), None);
    }

    #[test]
    fn empty_object_is_valid() {
        let p = StatusPayload::parse("{}").expect("empty object parses");
        assert!(p.display_dir().is_none());
        assert!(p.five_hour().is_none());
    }

    /// A future Claude Code release adding keys must not break us.
    #[test]
    fn unknown_fields_are_ignored() {
        let p = StatusPayload::parse(
            r#"{"cwd":"/x","brand_new_key":{"nested":[1,2,3]},"model":{"id":"m","extra":true}}"#,
        )
        .expect("unknown keys tolerated");
        assert_eq!(p.display_dir(), Some("/x"));
        assert_eq!(p.model_name(), Some("m"));
    }

    /// rate_limits vanished from the payload once already; absence is normal.
    #[test]
    fn missing_rate_limits_degrades_to_none() {
        let p = StatusPayload::parse(r#"{"rate_limits":{}}"#).unwrap();
        assert_eq!(p.five_hour(), None);
        assert_eq!(p.seven_day(), None);

        let p = StatusPayload::parse(r#"{"rate_limits":{"five_hour":{}}}"#).unwrap();
        assert_eq!(p.five_hour().unwrap().used_percentage, None);
    }

    #[test]
    fn malformed_json_yields_none() {
        assert!(StatusPayload::parse("not json").is_none());
        assert!(StatusPayload::parse("").is_none());
        assert!(StatusPayload::parse(r#"{"cwd":"#).is_none());
    }

    /// Wrong types must not panic either — they just fail the whole parse.
    #[test]
    fn wrong_types_yield_none() {
        assert!(StatusPayload::parse(r#"{"cwd":123}"#).is_none());
    }
}
