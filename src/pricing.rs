//! API list prices, and what recorded usage would have cost at them.
//!
//! Rates verified against <https://platform.claude.com/docs/en/about-claude/pricing>
//! on 2026-09-13, in USD per million tokens.
//!
//! Two facts make pricing from a transcript possible at all:
//!
//! * **Long context is not a separate tier.** "Claude 4.6 and later models
//!   include the full 1M token context window at standard pricing. (A
//!   900k-token request is billed at the same per-token rate as a 9k-token
//!   request.)" So although `message.model` records `claude-opus-5` without the
//!   `[1m]` suffix that `cost-state` uses, the bare model id is enough to price
//!   a request — the context window does not change the rate.
//! * **The four token classes are priced differently**, and the ledger stores
//!   all of them separately, including the 1-hour vs 5-minute cache-write split
//!   (2x vs 1.25x base input). Folding those together would misprice the
//!   largest line on the bill.
//!
//! This is an estimate of **API list price**, not what a subscription charges.
//! A subscription's cost is its fee; this answers "what would this usage have
//! cost on the API", which is the number that says whether the subscription is
//! worth it.
//!
//! What it does *not* model, because the ledger has no traffic exhibiting any
//! of them: fast mode (`speed: "fast"` doubles Opus 5 rates), US-pinned
//! inference (`inference_geo: "us"` adds 10% to every class), and server tool
//! charges such as web search at $10 per 1,000 requests.

/// Per-million-token rates, USD.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rates {
    pub input: f64,
    pub cache_write_5m: f64,
    pub cache_write_1h: f64,
    pub cache_read: f64,
    pub output: f64,
}

/// What a set of token counts would have cost.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Cost {
    pub input: f64,
    pub output: f64,
    pub cache_write: f64,
    pub cache_read: f64,
}

impl Cost {
    pub fn total(&self) -> f64 {
        self.input + self.output + self.cache_write + self.cache_read
    }

    pub fn add(&mut self, other: Cost) {
        self.input += other.input;
        self.output += other.output;
        self.cache_write += other.cache_write;
        self.cache_read += other.cache_read;
    }
}

/// Rates for a model as they stood at `ts` (UTC seconds).
///
/// Prices are a function of `(model, date)`, not of model alone: Anthropic has
/// both cut a rate mid-life (Fable 5.1's cache reads dropped to 0.025x base on
/// 2026-09-01) and cancelled a scheduled rise (Sonnet 5's introductory $2/$10
/// was made permanent instead of stepping to $3/$15). A table without a date
/// axis silently prices last month's requests at this month's rates.
///
/// Every arm below currently returns one rate for all time, because no change
/// falls inside any window this ledger holds. The parameter exists so that when
/// one does, the fix is an added arm here rather than a refactor of every call
/// site under time pressure. The failure mode of "add the axis the day a rate
/// moves" is that nobody notices the day; the failure mode of carrying an
/// unused parameter is nothing.
///
/// Returns `None` for anything not in the table, so callers report unpriced
/// tokens rather than valuing them at zero — a model id introduced by a future
/// Claude Code release must not quietly shrink a total.
pub fn rates_for(model: &str, ts: i64) -> Option<Rates> {
    // Deliberately unused for now: no rate in this table has moved within a
    // window the ledger can represent. Branch on it here when one does.
    let _ = ts;

    // Claude Code records pinned ids; a few carry a date suffix.
    let id = model.strip_suffix("-20251001").unwrap_or(model);

    let r = |input, cache_read, output| Rates {
        input,
        cache_write_5m: input * 1.25,
        cache_write_1h: input * 2.0,
        cache_read,
        output,
    };

    Some(match id {
        // Cache reads are 0.1x base input, except Fable 5.1 / Mythos 5.1 at 0.025x.
        "claude-fable-5-1" | "claude-mythos-5-1" => r(10.0, 0.25, 50.0),
        "claude-fable-5" | "claude-mythos-5" => r(10.0, 1.00, 50.0),
        "claude-opus-5" | "claude-opus-4-8" | "claude-opus-4-7" | "claude-opus-4-6"
        | "claude-opus-4-5" => r(5.0, 0.50, 25.0),
        "claude-opus-4-1" | "claude-opus-4" => r(15.0, 1.50, 75.0),
        "claude-sonnet-5" => r(2.0, 0.20, 10.0),
        "claude-sonnet-4-6" | "claude-sonnet-4-5" | "claude-sonnet-4" => r(3.0, 0.30, 15.0),
        "claude-haiku-4-5" => r(1.0, 0.10, 5.0),
        "claude-haiku-3-5" => r(0.80, 0.08, 4.0),
        _ => return None,
    })
}

/// Price one request's token counts at the rates in force when it was made.
///
/// `cache_1h` and `cache_5m` are the ephemeral split; they carry different
/// multipliers and must not be summed before pricing.
pub fn cost_of(
    model: &str,
    ts: i64,
    input: i64,
    output: i64,
    cache_1h: i64,
    cache_5m: i64,
    cache_read: i64,
) -> Option<Cost> {
    let r = rates_for(model, ts)?;
    let per_m = |tokens: i64, rate: f64| (tokens as f64 / 1_000_000.0) * rate;

    Some(Cost {
        input: per_m(input, r.input),
        output: per_m(output, r.output),
        cache_write: per_m(cache_1h, r.cache_write_1h) + per_m(cache_5m, r.cache_write_5m),
        cache_read: per_m(cache_read, r.cache_read),
    })
}

/// `$1,234.56`, or `$0.0012` for amounts that would otherwise round to nothing.
pub fn format_usd(amount: f64) -> String {
    if amount == 0.0 {
        "$0.00".to_string()
    } else if amount.abs() < 0.01 {
        format!("${amount:.4}")
    } else {
        format!("${amount:.2}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-09-05, inside the window this ledger actually holds.
    const T: i64 = 1_788_616_592;

    fn approx(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-9, "{a} != {b}");
    }

    #[test]
    fn opus_rates_match_the_published_table() {
        let r = rates_for("claude-opus-5", T).unwrap();
        approx(r.input, 5.0);
        approx(r.cache_write_5m, 6.25);
        approx(r.cache_write_1h, 10.0);
        approx(r.cache_read, 0.50);
        approx(r.output, 25.0);
    }

    /// The published exception: Fable 5.1 cache reads are 0.025x base, not 0.1x.
    #[test]
    fn fable_5_1_has_the_discounted_cache_read_rate() {
        approx(rates_for("claude-fable-5-1", T).unwrap().cache_read, 0.25);
        approx(rates_for("claude-fable-5", T).unwrap().cache_read, 1.00);
    }

    #[test]
    fn cache_write_multipliers_are_derived_not_guessed() {
        for model in ["claude-opus-5", "claude-sonnet-5", "claude-haiku-4-5"] {
            let r = rates_for(model, T).unwrap();
            approx(r.cache_write_5m, r.input * 1.25);
            approx(r.cache_write_1h, r.input * 2.0);
        }
    }

    #[test]
    fn dated_model_ids_resolve() {
        assert_eq!(
            rates_for("claude-haiku-4-5-20251001", T),
            rates_for("claude-haiku-4-5", T)
        );
    }

    /// An unknown model must not be priced at zero — silently shrinking the
    /// total is worse than reporting that some tokens could not be priced.
    #[test]
    fn unknown_models_are_not_priced() {
        assert!(rates_for("claude-something-6", T).is_none());
        assert!(cost_of("claude-something-6", T, 1, 1, 1, 1, 1).is_none());
    }

    /// The worked example from the pricing docs: 10k uncached input, 40k cache
    /// reads, 15k output on Opus 5 = $0.05 + $0.02 + $0.375.
    #[test]
    fn matches_the_published_worked_example() {
        let c = cost_of("claude-opus-5", T, 10_000, 15_000, 0, 0, 40_000).unwrap();
        approx(c.input, 0.05);
        approx(c.cache_read, 0.02);
        approx(c.output, 0.375);
        approx(c.total(), 0.445);
    }

    /// The split matters: 1h writes cost 2x base, 5m writes 1.25x. Summing them
    /// before pricing would understate a 1h-only workload by 37.5%.
    #[test]
    fn ephemeral_split_is_priced_separately() {
        let all_1h = cost_of("claude-opus-5", T, 0, 0, 1_000_000, 0, 0).unwrap();
        let all_5m = cost_of("claude-opus-5", T, 0, 0, 0, 1_000_000, 0).unwrap();
        approx(all_1h.cache_write, 10.0);
        approx(all_5m.cache_write, 6.25);
        assert!(all_1h.cache_write > all_5m.cache_write);
    }

    #[test]
    fn costs_accumulate() {
        let mut total = Cost::default();
        total.add(cost_of("claude-opus-5", T, 0, 1_000_000, 0, 0, 0).unwrap());
        total.add(cost_of("claude-opus-5", T, 0, 1_000_000, 0, 0, 0).unwrap());
        approx(total.output, 50.0);
        approx(total.total(), 50.0);
    }

    /// The date axis is wired through even though no arm branches on it yet:
    /// a request from any point in the ledger's range prices identically today,
    /// and that equality is what a future rate change is expected to break.
    #[test]
    fn rates_are_currently_stable_across_the_ledgers_whole_range() {
        let earliest = 1_788_000_000; // before any data this ledger holds
        let latest = 1_800_000_000; // comfortably after
        for model in ["claude-opus-5", "claude-fable-5-1", "claude-sonnet-5"] {
            assert_eq!(
                rates_for(model, earliest),
                rates_for(model, latest),
                "{model} has no dated arm yet; add one when a rate moves"
            );
        }
    }

    #[test]
    fn small_amounts_stay_visible() {
        assert_eq!(format_usd(0.0), "$0.00");
        assert_eq!(format_usd(0.0012), "$0.0012");
        assert_eq!(format_usd(12.5), "$12.50");
        assert_eq!(format_usd(510.617), "$510.62");
    }
}
